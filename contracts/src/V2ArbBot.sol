// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import "openzeppelin-contracts/token/ERC20/IERC20.sol";
import "openzeppelin-contracts/token/ERC20/utils/SafeERC20.sol";

import "./interface/IWETH.sol";
import "./interface/IUniswapV2.sol";
import "./interface/IBalancer.sol";
import "./interface/IAaveV3.sol";
import "./interface/IAlgebraPool.sol";
import "./interface/IUniswapV3Pool.sol";
import "./interface/ILBPair.sol";
import "./interface/IMorpho.sol";

/// @dev Minimal extension of IERC20 to access decimals() for stable AMM math.
interface IERC20Extended is IERC20 {
    function decimals() external view returns (uint8);
}

contract V2ArbBot is IFlashLoanRecipient, IUniswapV2Callee, IFlashLoanSimpleReceiver, IUniswapV3SwapCallback, IUniswapV3FlashCallback, IMorphoFlashLoanCallback {
    // can perform flashloan, multihop swaps in Uniswap V2 variant pools
    using SafeERC20 for IERC20;

    address public immutable owner;
    IWETH public immutable mainCurrency;
    mapping(address => bool) public allowedFlashAssets;

    /// @dev Trusted Aave V3 pool address — set by owner, checked in executeOperation.
    address public aavePool;
    /// @dev Trusted UniswapV3 pool used as flash lender for arb start token.
    address public uniV3FlashPool;
    /// @dev Trusted PancakeSwapV3 pool used as flash lender for arb start token.
    address public pancakeV3FlashPool;

    /// @dev Transient auth slots for CL swap callbacks — set to the pool being swapped
    ///      before pool.swap() and cleared immediately after.  Prevents any EOA from
    ///      calling the callbacks and draining contract tokens.
    address private _pendingAlgebraPool;
    address private _pendingV3Pool;

    /// @dev Liquidation transient auth — set before Balancer flash loan, cleared inside receiveFlashLoan.
    bool    private _liquidationPending;
    address private _liquidationVault;

    /// @dev Morpho flash loan transient auth — set before flashLoan(), cleared in onMorphoFlashLoan.
    bool    private _morphoLiquidationPending;
    /// @dev Balancer vault used for swap legs when liquidating via Morpho flash loan.
    ///      address(0) = fall back to Uniswap V3 for all swap legs.
    address private _pendingSwapVault;

    /// @dev AAVE V3 flash loan liquidation transient auth — used as last-resort fallback.
    bool    private _aaveLiquidationPending;
    /// @dev Swap vault passed to _executeLiquidationCore during AAVE flash loan callback.
    address private _pendingAaveLiqSwapVault;

    /// @dev Morpho Blue position liquidation transient auth.
    ///      Set before the Balancer flash loan, cleared inside receiveFlashLoan.
    bool    private _morphoBlueLiqPending;
    /// @dev Morpho Blue singleton address stored during the Morpho Blue liq flow.
    address private _morphoBlueAddr;

    // ── Arb execution parameters — must match the Rust bot's FlashSwap::SwapParams ──
    //
    //   poolVersions encoding:
    //     0 = UniswapV2 / PancakeSwapV2 / Solidly-volatile (constant-product)
    //     1 = UniswapV3 / PancakeSwapV3 (concentrated liquidity, V3 callback)
    //     2 = Algebra V1.9 CL pools (Thena Fusion — algebraSwapCallback)
    //     3 = Solidly stable AMM     (x³y+xy³=k, same swap() ABI as V2)
    //
    //   fees[i] = swap fee in basis points for hop i.
    //     V2/Solidly-stable only; V3 and Algebra pools ignore this field.
    //     e.g. 25 = PancakeSwap V2 0.25%  |  30 = UniSwap V2 0.30%
    struct SwapParams {
        address[] pools;        // pool address per hop
        uint8[]   poolVersions; // protocol type (see encoding above)
        uint32[]  fees;         // fee in basis-points per hop
        uint256   amountIn;     // start amount in start-token units
        address   startToken;   // explicit start token (avoids ambiguity in 2-hop cycles)
        uint8     flashLoanProvider; // 0=AaveV3, 1=UniswapV3, 2=PancakeSwapV3, 255=direct
    }

    // ── AAVE V3 liquidation parameters ───────────────────────────────────────
    struct LiquidationParams {
        address user;
        address collateralAsset;
        address debtAsset;
        uint256 debtToCover;
        /// @dev Uniswap V3 pool address for collateral→WETH hop (address(0) if collateral is WETH).
        address collateralPool;
        /// @dev Uniswap V3 pool address for WETH→debt hop (address(0) if debt is WETH).
        address debtPool;
        /// @dev Balancer V2 poolId for collateral→WETH swap.
        ///      Non-zero takes priority over collateralPool (0% protocol fee).
        ///      bytes32(0) → fall back to Uniswap V3 collateralPool.
        bytes32 colBalancerPool;
        /// @dev Balancer V2 poolId for WETH→debt swap.
        ///      Non-zero takes priority over debtPool (0% protocol fee).
        ///      bytes32(0) → fall back to Uniswap V3 debtPool.
        bytes32 debtBalancerPool;
    }

    receive() external payable {
        // wrap on receive
        mainCurrency.deposit{value: msg.value}();
    }

    constructor(address _owner, address _mainCurrency) {
        // mainCurrency on Ethereum is WETH
        // mainCurrency on Polygon is WMATIC
        owner = _owner;
        mainCurrency = IWETH(_mainCurrency);
    }

    function setAllowedFlashAsset(address asset, bool allowed) external {
        require(msg.sender == owner, "not owner");
        allowedFlashAssets[asset] = allowed;
    }

    /// @notice Set the trusted Aave V3 pool address (checked in executeOperation).
    function setAavePool(address _aavePool) external {
        require(msg.sender == owner, "not owner");
        aavePool = _aavePool;
    }

    function setUniV3FlashPool(address _pool) external {
        require(msg.sender == owner, "not owner");
        uniV3FlashPool = _pool;
    }

    function setPancakeV3FlashPool(address _pool) external {
        require(msg.sender == owner, "not owner");
        pancakeV3FlashPool = _pool;
    }

    function recoverToken(address token) public payable {
        require(msg.sender == owner, "not owner");
        IERC20(token).safeTransfer(
            msg.sender,
            IERC20(token).balanceOf(address(this)) - 1
        );
    }

    function approveRouter(
        address router,
        address[] memory tokens,
        bool force
    ) public {
        require(msg.sender == owner, "not owner");
        // skip approval if it already has allowance and if force is false
        uint maxInt = type(uint256).max;

        uint tokensLength = tokens.length;

        for (uint i; i < tokensLength; ) {
            IERC20 token = IERC20(tokens[i]);
            uint allowance = token.allowance(address(this), router);
            if (allowance < (maxInt / 2) || force) {
                // USDT-style tokens revert on approve() when current allowance > 0.
                // Reset to zero first, then set to max.
                token.safeApprove(router, 0);
                token.safeApprove(router, maxInt);
            }

            unchecked {
                i++;
            }
        }
    }

    // ── External arb entry point ──────────────────────────────────────────────

    /// @notice Primary arbitrage entry point called by the Rust bot.
    /// @dev    Encodes the SwapParams and initiates an Aave V3 flash loan for the
    ///         start token.  Inside executeOperation() the swap path is executed
    ///         and the loan repaid.  Any surplus stays in the contract.
    ///
    ///         If Aave is not configured (aavePool == address(0)) or the start
    ///         token is not in allowedFlashAssets, falls back to direct execution
    ///         (the contract must already hold sufficient start-token balance).
    function executeArbitrage(SwapParams calldata arb) external {
        require(msg.sender == owner, "not owner");
        require(arb.pools.length > 0, "empty path");
        require(arb.pools.length == arb.poolVersions.length, "len mismatch");

        address startToken = arb.startToken != address(0)
            ? arb.startToken
            : _resolveStartToken(arb.pools[0]);

        if (arb.flashLoanProvider == 0) {
            require(aavePool != address(0), "aave not configured");
            require(allowedFlashAssets[startToken], "asset not allowed");
            IAaveV3Pool(aavePool).flashLoanSimple(
                address(this),
                startToken,
                arb.amountIn,
                abi.encode(arb),
                0
            );
            return;
        }

        if (arb.flashLoanProvider == 1) {
            require(uniV3FlashPool != address(0), "uni v3 flash pool not configured");
            _executeArbWithFlashPool(uniV3FlashPool, startToken, arb);
            return;
        }

        if (arb.flashLoanProvider == 2) {
            require(pancakeV3FlashPool != address(0), "pancake v3 flash pool not configured");
            _executeArbWithFlashPool(pancakeV3FlashPool, startToken, arb);
            return;
        }

        // Direct path fallback (flashLoanProvider=255 or unknown)
        uint256 bal = IERC20(startToken).balanceOf(address(this));
        require(bal >= arb.amountIn, "insufficient balance for direct arb");
        uint256 amountOut = _executeSwapPath(arb, startToken, arb.amountIn);
        require(amountOut > arb.amountIn, "direct arb: no profit");
    }

    function _executeArbWithFlashPool(
        address flashPool,
        address startToken,
        SwapParams calldata arb
    ) internal {
        address token0 = IUniswapV3Pool(flashPool).token0();
        address token1 = IUniswapV3Pool(flashPool).token1();

        uint256 amount0 = startToken == token0 ? arb.amountIn : 0;
        uint256 amount1 = startToken == token1 ? arb.amountIn : 0;
        require(amount0 > 0 || amount1 > 0, "start token not in flash pool");

        IUniswapV3Pool(flashPool).flash(
            address(this),
            amount0,
            amount1,
            abi.encode(arb, flashPool, startToken)
        );
    }

    /// @dev Determine the start token for an arb.  The start token is whichever
    ///      of pool0's tokens matches mainCurrency (WBNB / WETH on the target chain).
    ///      If neither matches — unusual but possible for non-native start tokens —
    ///      token0 is used as a default.
    function _resolveStartToken(address pool) internal view returns (address) {
        address t0   = IUniswapV2Pair(pool).token0();
        address t1   = IUniswapV2Pair(pool).token1();
        address main = address(mainCurrency);
        if (t0 == main || t1 == main) return main;
        return t0; // non-native arb: assume token0 is the start token
    }

    /// @dev Execute a full multi-hop swap path.
    ///      At entry this contract holds `startAmt` of `startToken`.
    ///      Returns the total amount of startToken (or the last hop's output token)
    ///      held by this contract after all hops complete.
    ///
    ///      Pool-version dispatch:
    ///        0 → _swapV2Pair         (UniV2 / PCS V2 / Solidly volatile)
    ///        1 → _swapUniswapV3Pool  (UniV3 / PCS V3 — uniswapV3SwapCallback)
    ///        2 → _swapAlgebraPool    (Thena Fusion / AlgebraV1.9 — algebraSwapCallback)
    ///        3 → _swapSolidlyStablePair (x³y+xy³=k, reads token decimals at runtime)
    function _executeSwapPath(
        SwapParams memory arb,
        address startToken,
        uint256 startAmt
    ) internal returns (uint256) {
        address currentToken = startToken;
        uint256 currentAmt   = startAmt;
        uint256 nhop         = arb.pools.length;

        for (uint256 i = 0; i < nhop; ) {
            address pool    = arb.pools[i];
            uint8   version = arb.poolVersions[i];
            // fee in basis-points for this hop (default 25 = PancakeSwap V2 0.25%).
            // V3 and Algebra pools ignore this value.
            uint32  feeBps  = (i < arb.fees.length) ? arb.fees[i] : 25;

            // Determine output token by consulting the pool's token pair.
            // token0() / token1() is supported by V2, V3, and Algebra pools.
            address t0        = IUniswapV2Pair(pool).token0();
            address nextToken = (currentToken == t0)
                ? IUniswapV2Pair(pool).token1()
                : t0;

            if (version == 0) {
                // UniswapV2-style constant-product pair.
                currentAmt = _swapV2Pair(pool, currentToken, nextToken, currentAmt, feeBps);
            } else if (version == 1) {
                // UniswapV3 / PancakeSwap V3 concentrated-liquidity pool.
                currentAmt = _swapUniswapV3Pool(pool, currentToken, nextToken, currentAmt);
            } else if (version == 2) {
                // Algebra V1.9 concentrated-liquidity pool (Thena Fusion).
                currentAmt = _swapAlgebraPool(pool, currentToken, nextToken, currentAmt);
            } else if (version == 3) {
                // Solidly stable AMM (x³y+xy³=k).  Reads token decimals at runtime.
                currentAmt = _swapSolidlyStablePair(pool, currentToken, nextToken, currentAmt, feeBps);
            } else {
                revert("unknown pool version");
            }

            currentToken = nextToken;
            unchecked { i++; }
        }

        return currentAmt;
    }

    function _swapV2Pair(
        address pairAddr,
        address tokenIn,
        address tokenOut,
        uint amountIn,
        uint feeBps
    ) internal returns (uint amountOut) {
        IUniswapV2Pair pair = IUniswapV2Pair(pairAddr);
        (uint112 r0, uint112 r1, ) = pair.getReserves();
        address token0 = pair.token0();

        uint reserveIn = tokenIn == token0 ? uint(r0) : uint(r1);
        uint reserveOut = tokenIn == token0 ? uint(r1) : uint(r0);

        // Dynamic fee: base = 10000 - feeBps (e.g. 9929 for Blackhole 71 bps, 9700 for std V2 300 bps)
        uint base = 10000 - feeBps;
        uint amountInWithFee = amountIn * base;
        uint numerator = amountInWithFee * reserveOut;
        uint denominator = (reserveIn * 10000) + amountInWithFee;
        uint amountExpected = numerator / denominator;

        // Snapshot tokenOut balance before the swap so we can measure actual receipt.
        // Some V2 variants (e.g. Blackhole) deduct a protocol fee from the output,
        // meaning the bot receives less than amountExpected.  Using the delta ensures
        // the next hop never tries to spend more than what was actually received.
        uint balanceBefore = IERC20(tokenOut).balanceOf(address(this));

        IERC20(tokenIn).safeTransfer(address(pair), amountIn);

        if (tokenIn == token0) {
            pair.swap(0, amountExpected, address(this), new bytes(0));
        } else {
            pair.swap(amountExpected, 0, address(this), new bytes(0));
        }

        // Actual received amount (may be less than amountExpected due to protocol fees)
        amountOut = IERC20(tokenOut).balanceOf(address(this)) - balanceBefore;
    }

    function _swapAlgebraPool(
        address poolAddr,
        address tokenIn,
        address tokenOut,
        uint amountIn
    ) internal returns (uint amountOut) {
        // Snapshot tokenOut balance to handle fee-on-transfer tokens correctly.
        uint balanceBefore = IERC20(tokenOut).balanceOf(address(this));

        // Arm callback auth before calling pool.swap(); disarm after.
        _pendingAlgebraPool = poolAddr;
        IAlgebraPool pool = IAlgebraPool(poolAddr);
        bool zeroToOne = (tokenIn == pool.token0());

        uint160 limit = zeroToOne
            ? 4295128740                                                    // MIN_SQRT_RATIO + 1 (Algebra MIN_SQRT_RATIO = 4295128739)
            : 1461446703485210103287273052203988822378723970341;            // MAX_SQRT_RATIO - 1

        pool.swap(
            address(this),
            zeroToOne,
            int256(amountIn),
            limit,
            abi.encode(tokenIn, amountIn)
        );

        _pendingAlgebraPool = address(0);
        // Actual received amount via balance delta (FoT-safe).
        amountOut = IERC20(tokenOut).balanceOf(address(this)) - balanceBefore;
    }

    // ── Solidly stable pair (x³y + xy³ = k) ─────────────────────────────────
    // The swap() ABI is identical to Uniswap V2 — we just need to compute
    // amountOut correctly on-chain using the Solidly invariant.
    function _getAmountOutStable(
        uint amountIn,
        uint reserveIn,
        uint reserveOut,
        uint decimalsIn,
        uint decimalsOut
    ) internal pure returns (uint) {
        // Scale reserves to 18-decimal precision
        uint scaleIn  = 10 ** (18 - decimalsIn);
        uint scaleOut = 10 ** (18 - decimalsOut);
        uint x0 = reserveIn  * scaleIn;
        uint y0 = reserveOut * scaleOut;
        uint dx = amountIn   * scaleIn;
        uint x1 = x0 + dx;

        // Compute invariant k = x0*y0*(x0²+y0²) normalised by 1e54 to avoid
        // overflow.  Using staged mulDiv matches the Solidly/Aerodrome reference.
        // k = (x0*y0/1e18) * (x0²/1e18 + y0²/1e18) / 1e18
        uint k = mulDiv(
            mulDiv(x0, y0, 1e18),
            mulDiv(x0, x0, 1e18) + mulDiv(y0, y0, 1e18),
            1e18
        );

        // Newton-Raphson: find y1 s.t. f(x1, y1) == k (same normalisation)
        // f(y)  = (x1*y/1e18) * (x1²/1e18 + y²/1e18) / 1e18
        // f'(y) = x1 * (x1²/1e18 + 3*y²/1e18) / 1e18
        uint y = y0;
        for (uint i; i < 255; ) {
            uint fNum  = mulDiv(x1, y, 1e18);
            uint fNum2 = mulDiv(fNum, mulDiv(x1, x1, 1e18) + mulDiv(y, y, 1e18), 1e18);
            if (fNum2 >= k) {
                uint excess = fNum2 - k;
                uint fDer   = mulDiv(x1, mulDiv(x1, x1, 1e18) + mulDiv(3 * y, y, 1e18), 1e18);
                if (fDer == 0) break;
                uint step = mulDiv(excess, 1e18, fDer);
                if (step == 0) break;
                y = y > step ? y - step : 0;
            } else {
                uint deficit = k - fNum2;
                uint fDer    = mulDiv(x1, mulDiv(x1, x1, 1e18) + mulDiv(3 * y, y, 1e18), 1e18);
                if (fDer == 0) break;
                uint step = mulDiv(deficit, 1e18, fDer);
                if (step == 0) break;
                y = y + step;
            }
            unchecked { i++; }
        }

        uint dy = y0 > y ? y0 - y : 0;
        return dy / scaleOut;
    }

    /// @dev 512-bit safe mulDiv — prevents overflow on large Solidly stable reserves.
    ///      Equivalent to Uniswap v3 FullMath.mulDiv: computes floor(a*b/denom)
    ///      without overflow by using a 512-bit intermediate (two 256-bit words).
    function mulDiv(uint256 a, uint256 b, uint256 denom) internal pure returns (uint256 result) {
        require(denom > 0, "mulDiv: denom 0");
        unchecked {
            // Compute 512-bit product [prod1 prod0] = a * b
            uint256 prod0;
            uint256 prod1;
            assembly {
                let mm := mulmod(a, b, not(0))
                prod0 := mul(a, b)
                prod1 := sub(sub(mm, prod0), lt(mm, prod0))
            }
            // If high word is zero the result fits in 256 bits — fast path
            if (prod1 == 0) {
                result = prod0 / denom;
                return result;
            }
            // Result must fit in 256 bits
            require(prod1 < denom, "mulDiv: overflow");
            // Subtract remainder so division is exact
            uint256 remainder;
            assembly { remainder := mulmod(a, b, denom) }
            assembly {
                prod1 := sub(prod1, gt(remainder, prod0))
                prod0 := sub(prod0, remainder)
            }
            // Factor powers of two out of denom and shift dividend accordingly
            uint256 twos;
            assembly { twos := and(sub(0, denom), denom) }
            assembly { denom := div(denom, twos) }
            assembly { prod0 := div(prod0, twos) }
            assembly { prod0 := or(prod0, mul(prod1, add(div(sub(0, twos), twos), 1))) }
            // Compute modular inverse of denom mod 2^256 via Newton iterations
            uint256 inv = (3 * denom) ^ 2;
            inv *= 2 - denom * inv;
            inv *= 2 - denom * inv;
            inv *= 2 - denom * inv;
            inv *= 2 - denom * inv;
            inv *= 2 - denom * inv;
            inv *= 2 - denom * inv;
            result = prod0 * inv;
        }
    }

    function _swapSolidlyStablePair(
        address pairAddr,
        address tokenIn,
        address tokenOut,
        uint    amountIn,
        uint    feeBps
    ) internal returns (uint amountOut) {
        IUniswapV2Pair pair = IUniswapV2Pair(pairAddr);
        (uint112 r0, uint112 r1, ) = pair.getReserves();
        address token0 = pair.token0();

        bool zeroForOne = (tokenIn == token0);
        uint reserveIn  = zeroForOne ? uint(r0) : uint(r1);
        uint reserveOut = zeroForOne ? uint(r1) : uint(r0);

        // Get decimals for stable AMM normalisation
        uint dIn  = uint(IERC20Extended(tokenIn).decimals());
        uint dOut = uint(IERC20Extended(tokenOut).decimals());

        // Deduct fee before computing invariant (same as Solidly reference impl).
        uint amountInWithFee = amountIn * (10000 - feeBps) / 10000;
        amountOut = _getAmountOutStable(amountInWithFee, reserveIn, reserveOut, dIn, dOut);
        require(amountOut > 0, "stable: zero out");

        IERC20(tokenIn).safeTransfer(pairAddr, amountIn);

        if (zeroForOne) {
            pair.swap(0, amountOut, address(this), new bytes(0));
        } else {
            pair.swap(amountOut, 0, address(this), new bytes(0));
        }
    }

    // ── LFJ Liquidity Book V2.1/V2.2 ─────────────────────────────────────────
    /// @notice Swap on an LFJ Liquidity Book pair.
    /// @dev    Transfer tokenIn to the pair before calling; measures output via balance snapshot.
    function _swapLFJPair(
        address pairAddr,
        address tokenIn,
        address tokenOut,
        uint    amountIn
    ) internal returns (uint amountOut) {
        ILBPair pair = ILBPair(pairAddr);

        // swapForY = true means tokenIn is tokenX (sell X, buy Y).
        address tokenX = pair.getTokenX();
        bool swapForY = (tokenIn == tokenX);

        // Snapshot tokenOut balance before swap.
        uint balanceBefore = IERC20(tokenOut).balanceOf(address(this));

        // Transfer input tokens to the pair.
        IERC20(tokenIn).safeTransfer(pairAddr, amountIn);

        // Execute swap — result is packed bytes32 (high 128 = amountY, low 128 = amountX).
        pair.swap(swapForY, address(this));

        // Actual received amount via balance delta.
        amountOut = IERC20(tokenOut).balanceOf(address(this)) - balanceBefore;
    }

    // ── Uniswap V3 CL (Aerodrome CL, PancakeSwap V3, and all V3 forks) ─────────
    function _swapUniswapV3Pool(
        address poolAddr,
        address tokenIn,
        address tokenOut,
        uint    amountIn
    ) internal returns (uint amountOut) {
        // Snapshot tokenOut balance to handle fee-on-transfer tokens correctly.
        uint balanceBefore = IERC20(tokenOut).balanceOf(address(this));

        // Arm callback auth before calling pool.swap(); disarm after.
        _pendingV3Pool = poolAddr;
        IUniswapV3Pool pool = IUniswapV3Pool(poolAddr);
        bool zeroForOne = (tokenIn == pool.token0());

        // Standard UniV3 price limits
        uint160 limit = zeroForOne
            ? 4295128740                                          // TickMath.MIN_SQRT_RATIO + 1
            : 1461446703485210103287273052203988822378723970341;  // TickMath.MAX_SQRT_RATIO - 1

        pool.swap(
            address(this),
            zeroForOne,
            int256(amountIn),
            limit,
            abi.encode(tokenIn, amountIn)
        );

        _pendingV3Pool = address(0);
        // Actual received amount via balance delta (FoT-safe).
        amountOut = IERC20(tokenOut).balanceOf(address(this)) - balanceBefore;
    }

    /// @notice Uniswap V3 swap callback — called by UniV3 / Aerodrome CL pools.
    function uniswapV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external override {
        _handleV3Callback(amount0Delta, amount1Delta, data);
    }

    /// @notice PancakeSwap V3 swap callback — PCS V3 uses a different selector.
    function pancakeV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external {
        _handleV3Callback(amount0Delta, amount1Delta, data);
    }

    /// @dev Shared logic for all V3-style swap callbacks.
    function _handleV3Callback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) internal {
        // Only the pool we just called may invoke this callback.
        require(
            _pendingV3Pool != address(0) && msg.sender == _pendingV3Pool,
            "V3CB: not pool"
        );
        (address tokenIn, ) = abi.decode(data, (address, uint256));
        uint256 actualAmount = uint256(amount0Delta > 0 ? amount0Delta : amount1Delta);
        IERC20(tokenIn).safeTransfer(msg.sender, actualAmount);
    }

    function _execute(bytes memory data) internal returns (uint amountOut) {
        uint8 nhop;
        uint256 minOut;

        assembly {
            // Header : amountIn (32), flashloan (32), loanFrom (32), minOut (32) = 128 bytes (0x80)
            // PathParam: router (32), tokenIn (32), tokenOut (32), poolType (32), fee (32) = 160 bytes (0xa0)
            // nhop = (len - 0x80) / 0xa0

            let len := mload(data)
            nhop := div(sub(len, 0x80), 0xa0)

            let offset := add(data, 0x20)
            amountOut := mload(offset)
            minOut := mload(add(offset, 0x60))  // 4th header slot
        }

        for (uint8 i; i < nhop; ) {
            address router;
            address tokenIn;
            address tokenOut;
            uint256 poolType;
            uint256 feeBps;

            assembly {
                // data[0x20] = first byte after the length prefix
                // header = 4 slots (0x80), so params start at data + 0x20 + 0x80 = data + 0xa0
                // each hop is 0xa0 bytes (5 slots)
                let offset := add(data, 0xa0)
                offset := add(offset, mul(0xa0, i))

                router   := mload(offset)
                tokenIn  := mload(add(offset, 0x20))
                tokenOut := mload(add(offset, 0x40))
                poolType := mload(add(offset, 0x60))
                feeBps   := mload(add(offset, 0x80))
            }

            // Logic to handle pool type
            if (poolType == 1) {
                // Algebra CL - call pool directly (bypass router)
                // router field holds pool address; fee is handled internally by the pool
                amountOut = _swapAlgebraPool(router, tokenIn, tokenOut, amountOut);
            } else if (poolType == 2) {
                // Solidly stable pair (x³y + xy³ = k) — same swap() ABI but different math.
                // router field holds pair address; feeBps carries the pool fee.
                amountOut = _swapSolidlyStablePair(router, tokenIn, tokenOut, amountOut, feeBps);
            } else if (poolType == 3) {
                // UniswapV3 CL (Aerodrome CL, PancakeSwap V3, etc.) — pool_type=3.
                // router field holds pool address; fee handled internally by the pool.
                amountOut = _swapUniswapV3Pool(router, tokenIn, tokenOut, amountOut);
            } else if (poolType == 4) {
                // LFJ Liquidity Book V2.1/V2.2 — call pair directly.
                // router field holds pair address.
                amountOut = _swapLFJPair(router, tokenIn, tokenOut, amountOut);
            } else {
                // V2 direct pair swap: `router` is the pair address
                amountOut = _swapV2Pair(router, tokenIn, tokenOut, amountOut, feeBps);
            }

            unchecked {
                i++;
            }
        }

        // Slippage protection: revert early if output is below the minimum
        // expected by the off-chain simulation.  This avoids wasting gas on
        // transactions that would be unprofitable.
        if (minOut > 0) {
            require(amountOut >= minOut, "slippage");
        }
    }

    // ── Morpho Blue position liquidation parameters ──────────────────────────

    /// @dev Flash-loan-backed Morpho Blue liquidation parameters.
    ///      Passed encoded in the Balancer flashLoan data field.
    struct MorphoBlueLiqParams {
        address loanToken;
        address collateralToken;
        /// @dev Market oracle — used by Morpho to price collateral; not called by this contract.
        address oracle;
        /// @dev Interest rate model address (part of MarketParams identity).
        address irm;
        /// @dev Loan-to-value ratio for this market (WAD scale: 1e18 = 100%).
        uint256 lltv;
        address borrower;
        /// @dev Borrow shares to repay; Morpho computes the collateral seized (incl. incentive).
        uint256 repaidShares;
        /// @dev Pre-computed flash loan amount in loan-token units (repaidAssets estimate).
        uint256 flashAmount;
        /// @dev Uniswap V3 pool for collateral→WETH leg (address(0) if collateral==WETH).
        address collateralPool;
        /// @dev Uniswap V3 pool for WETH→loanToken leg (address(0) if loanToken==WETH).
        address debtPool;
        /// @dev Balancer V2 poolId for collateral→WETH (bytes32(0) → use V3 collateralPool).
        bytes32 colBalancerPool;
        /// @dev Balancer V2 poolId for WETH→loanToken (bytes32(0) → use V3 debtPool).
        bytes32 debtBalancerPool;
    }

    function receiveFlashLoan(
        IERC20[] memory tokens,
        uint[] memory amounts,
        uint[] memory feeAmounts,
        bytes memory data
    ) external override {
        // ── AAVE V3 liquidation path (Balancer flash loan) ───────────────────
        if (_liquidationPending) {
            require(msg.sender == _liquidationVault, "not vault");
            _liquidationPending = false;
            _liquidationVault   = address(0);
            _executeLiquidationFlashLoan(
                address(tokens[0]), amounts[0], feeAmounts[0], data
            );
            return;
        }

        // ── Morpho Blue position liquidation path (Balancer flash loan) ──────
        if (_morphoBlueLiqPending) {
            require(msg.sender == _liquidationVault, "not vault");
            _morphoBlueLiqPending = false;
            address morpho    = _morphoBlueAddr;
            address swapVault = _pendingSwapVault;
            _liquidationVault = address(0);
            _morphoBlueAddr   = address(0);
            _pendingSwapVault = address(0);
            MorphoBlueLiqParams memory p = abi.decode(data, (MorphoBlueLiqParams));
            _executeMorphoBlueLiqCore(morpho, swapVault, p, amounts[0], feeAmounts[0]);
            return;
        }

        // ── Arb path (unchanged) ─────────────────────────────────────────────
        address vault;
        assembly {
            let offset := add(data, 0x20)
            vault := mload(add(offset, 0x40))
        }
        require(msg.sender == vault, "not vault");

        IERC20 token = tokens[0];
        uint amountIn = amounts[0];

        // we don't need any amountOut checks for this
        // because if we can't pay back the loan, our function simply reverts
        _execute(data);

        // Repay principal + Balancer fee (feeAmounts[0] > 0 on some Balancer deployments).
        // Using safeTransfer to support non-standard ERC20 tokens (e.g. USDT).
        token.safeTransfer(vault, amountIn + feeAmounts[0]);
    }

    // ── Flash-loan-based AAVE V3 liquidation ──────────────────────────────────

    /// @notice Flash-borrow `p.debtToCover` of `p.debtAsset` from Balancer,
    ///         liquidate `p.user` on AAVE v3, swap received collateral back to
    ///         the debt asset, and repay the flash loan.
    ///         Any surplus stays in the contract (use recoverToken to sweep it).
    function triggerLiquidation(
        address balancerVault,
        LiquidationParams calldata p
    ) external {
        require(msg.sender == owner, "not owner");
        _liquidationPending = true;
        _liquidationVault   = balancerVault;

        IERC20[] memory tokens = new IERC20[](1);
        tokens[0] = IERC20(p.debtAsset);

        uint256[] memory amounts = new uint256[](1);
        amounts[0] = p.debtToCover;

        IBalancerVault(balancerVault).flashLoan(
            IFlashLoanRecipient(address(this)),
            tokens,
            amounts,
            abi.encode(p)
        );
    }

    // ── Shared liquidation core ────────────────────────────────────────────────

    /// @dev Core liquidation: approve AAVE, call liquidationCall, swap collateral → debt.
    ///      Does NOT handle flash loan repayment — callers do that themselves.
    ///
    /// @param swapVault Balancer vault address for Balancer-pool swap legs.
    ///                  Pass address(0) to skip Balancer and use Uniswap V3 for all legs.
    function _executeLiquidationCore(
        address swapVault,
        LiquidationParams memory p
    ) internal {
        // 1. Approve AAVE pool to pull the debt repayment.
        IERC20(p.debtAsset).safeApprove(aavePool, 0);
        IERC20(p.debtAsset).safeApprove(aavePool, p.debtToCover);

        // 2. Liquidate — receive collateralAsset underlying tokens.
        IAaveV3Pool(aavePool).liquidationCall(
            p.collateralAsset,
            p.debtAsset,
            p.user,
            p.debtToCover,
            false // receive underlying, not aToken
        );

        // 3. Swap collateral → debtAsset if they differ.
        if (p.collateralAsset != p.debtAsset) {
            address weth = address(mainCurrency);
            uint256 bal  = IERC20(p.collateralAsset).balanceOf(address(this));

            if (p.collateralAsset != weth) {
                if (p.colBalancerPool != bytes32(0) && swapVault != address(0)) {
                    // collateral → WETH via Balancer (0% protocol fee)
                    bal = _swapBalancer(swapVault, p.colBalancerPool, p.collateralAsset, weth, bal);
                } else if (p.collateralPool != address(0)) {
                    // collateral → WETH via Uniswap V3 (fallback)
                    bal = _swapUniswapV3Pool(p.collateralPool, p.collateralAsset, weth, bal);
                }
            }

            if (p.debtAsset != weth) {
                if (p.debtBalancerPool != bytes32(0) && swapVault != address(0)) {
                    // WETH → debtAsset via Balancer (0% protocol fee)
                    _swapBalancer(swapVault, p.debtBalancerPool, weth, p.debtAsset, bal);
                } else if (p.debtPool != address(0)) {
                    // WETH → debtAsset via Uniswap V3 (fallback)
                    _swapUniswapV3Pool(p.debtPool, weth, p.debtAsset, bal);
                }
            }
        }
    }

    /// @dev Executed inside receiveFlashLoan when _liquidationPending was set.
    ///      At entry this contract holds `debtAmount` of the debt asset.
    function _executeLiquidationFlashLoan(
        address debtToken,
        uint256 debtAmount,
        uint256 feeAmount,
        bytes memory data
    ) internal {
        LiquidationParams memory p = abi.decode(data, (LiquidationParams));

        // msg.sender here = Balancer Vault (still on the call stack).
        _executeLiquidationCore(msg.sender, p);

        // 4. Repay flash loan (principal + Balancer fee, usually 0).
        IERC20(debtToken).safeTransfer(msg.sender, debtAmount + feeAmount);
    }

    // ── Morpho flash-loan-backed AAVE V3 liquidation ──────────────────────────

    /// @notice Flash-borrow `p.debtToCover` of `p.debtAsset` from Morpho Blue
    ///         (0% fee), liquidate `p.user` on AAVE v3, swap received collateral
    ///         back to the debt asset, then repay the flash loan.
    ///         Any surplus stays in the contract (use recoverToken to sweep).
    ///
    /// @param morpho     Morpho Blue address (0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb on Base)
    /// @param swapVault  Balancer vault for Balancer swap legs; address(0) = V3-only path.
    /// @param p          Liquidation parameters (same struct as triggerLiquidation).
    function triggerLiquidationWithMorpho(
        address morpho,
        address swapVault,
        LiquidationParams calldata p
    ) external {
        require(msg.sender == owner, "not owner");
        _morphoLiquidationPending = true;
        _liquidationVault = morpho;
        _pendingSwapVault  = swapVault;

        IMorpho(morpho).flashLoan(p.debtAsset, p.debtToCover, abi.encode(p));
    }

    /// @inheritdoc IMorphoFlashLoanCallback
    /// @dev Called by Morpho Blue after it transfers tokens to this contract.
    ///      Executes the liquidation, then approves Morpho to pull back `assets`.
    ///      Morpho Blue fee = 0%, so we approve exactly `assets` with no premium.
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external override {
        require(
            _morphoLiquidationPending && msg.sender == _liquidationVault,
            "not morpho"
        );
        _morphoLiquidationPending = false;
        _liquidationVault  = address(0);
        address swapVault  = _pendingSwapVault;
        _pendingSwapVault  = address(0);

        LiquidationParams memory p = abi.decode(data, (LiquidationParams));

        // Execute core: liquidate + swap (uses V3 or Balancer depending on swapVault).
        _executeLiquidationCore(swapVault, p);

        // Morpho repayment: approve Morpho to pull back exactly `assets` (0% fee).
        IERC20(p.debtAsset).safeApprove(msg.sender, 0);
        IERC20(p.debtAsset).safeApprove(msg.sender, assets);
    }

    // ── Morpho Blue position liquidation (Balancer flash loan) ─────────────────

    /// @notice Flash-borrow `p.flashAmount` of `p.loanToken` from Balancer (0% fee),
    ///         liquidate `p.borrower` on Morpho Blue, swap received collateral back
    ///         to the loan token, then repay the flash loan.
    ///         Surplus (liquidation incentive spread) stays in the contract;
    ///         use recoverToken() to sweep profits.
    ///
    /// @param balancerVault  Balancer Vault address for the flash loan.
    /// @param morphoBlue     Morpho Blue singleton address.
    /// @param p              Liquidation parameters including pre-computed flash amount.
    function triggerMorphoBlueLiquidation(
        address balancerVault,
        address morphoBlue,
        MorphoBlueLiqParams calldata p
    ) external {
        require(msg.sender == owner, "not owner");
        _morphoBlueLiqPending = true;
        _morphoBlueAddr       = morphoBlue;
        _liquidationVault     = balancerVault; // checked in receiveFlashLoan
        _pendingSwapVault     = balancerVault; // swap legs use the same vault

        IERC20[] memory tokens  = new IERC20[](1);
        tokens[0]               = IERC20(p.loanToken);
        uint256[] memory amounts = new uint256[](1);
        amounts[0]              = p.flashAmount;

        IBalancerVault(balancerVault).flashLoan(
            IFlashLoanRecipient(address(this)),
            tokens,
            amounts,
            abi.encode(p)
        );
    }

    /// @dev Core execution for a Morpho Blue liquidation backed by a Balancer flash loan.
    ///      Called from receiveFlashLoan when _morphoBlueLiqPending was set.
    ///
    ///   1. Approve Morpho Blue for the flash-borrowed debt amount.
    ///   2. Call morpho.liquidate() — pay debt shares, receive collateral.
    ///   3. Swap collateral → (WETH) → loanToken via V3 or Balancer.
    ///   4. Repay Balancer flash loan (principal + fee, usually 0).
    function _executeMorphoBlueLiqCore(
        address morpho,
        address swapVault,
        MorphoBlueLiqParams memory p,
        uint256 debtAmount,
        uint256 feeAmount
    ) internal {
        // Build MarketParams struct for the liquidate() call
        IMorpho.MarketParams memory mp = IMorpho.MarketParams({
            loanToken:       p.loanToken,
            collateralToken: p.collateralToken,
            oracle:          p.oracle,
            irm:             p.irm,
            lltv:            p.lltv
        });

        // 1. Approve Morpho to pull the loan token (exact repaid amount)
        IERC20(p.loanToken).safeApprove(morpho, 0);
        IERC20(p.loanToken).safeApprove(morpho, debtAmount);

        // 2. Liquidate: repay borrow shares → receive collateral
        //    repaidShares set by caller; seizedAssets = 0 → Morpho computes amount.
        IMorpho(morpho).liquidate(mp, p.borrower, 0, p.repaidShares, "");

        // Clear any residual approval
        IERC20(p.loanToken).safeApprove(morpho, 0);

        // 3. Swap collateral → (WETH) → loanToken
        address weth = address(mainCurrency);
        if (p.collateralToken != p.loanToken) {
            uint256 colBal = IERC20(p.collateralToken).balanceOf(address(this));

            if (p.collateralToken != weth) {
                if (p.colBalancerPool != bytes32(0) && swapVault != address(0)) {
                    colBal = _swapBalancer(swapVault, p.colBalancerPool, p.collateralToken, weth, colBal);
                } else if (p.collateralPool != address(0)) {
                    colBal = _swapUniswapV3Pool(p.collateralPool, p.collateralToken, weth, colBal);
                }
            }

            if (p.loanToken != weth) {
                if (p.debtBalancerPool != bytes32(0) && swapVault != address(0)) {
                    _swapBalancer(swapVault, p.debtBalancerPool, weth, p.loanToken, colBal);
                } else if (p.debtPool != address(0)) {
                    _swapUniswapV3Pool(p.debtPool, weth, p.loanToken, colBal);
                }
            }
        }

        // 4. Repay Balancer flash loan (fee is 0% on Base)
        IERC20(p.loanToken).safeTransfer(msg.sender, debtAmount + feeAmount);
    }

    // ── AAVE V3 flash-loan-backed liquidation (last-resort, 0.05% fee) ────────

    /// @notice Flash-borrow `p.debtToCover` of `p.debtAsset` from AAVE V3
    ///         (0.05% fee), liquidate `p.user`, swap received collateral back
    ///         to the debt asset, then repay the flash loan.  Surplus stays
    ///         in the contract.
    ///
    /// @dev Uses the `aavePool` address already stored in the contract
    ///      (set via setAavePool).  Reverts if aavePool is not configured.
    ///
    /// @param swapVault Balancer vault for Balancer swap legs; address(0) = V3-only.
    /// @param p         Liquidation parameters (same struct as triggerLiquidation).
    function triggerLiquidationWithAave(
        address swapVault,
        LiquidationParams calldata p
    ) external {
        require(msg.sender == owner, "not owner");
        require(aavePool != address(0), "aave pool not set");
        _aaveLiquidationPending    = true;
        _pendingAaveLiqSwapVault   = swapVault;
        IAaveV3Pool(aavePool).flashLoanSimple(
            address(this),
            p.debtAsset,
            p.debtToCover,
            abi.encode(p),
            0 // referralCode
        );
    }

    /// @dev Single-pool swap through Balancer V2 Vault.
    ///      Approves the vault, executes GIVEN_IN swap, returns amountOut.
    ///      The Balancer protocol fee on Base is 0% so the full amountIn is converted.
    function _swapBalancer(
        address vault,
        bytes32 poolId,
        address tokenIn,
        address tokenOut,
        uint256 amountIn
    ) internal returns (uint256 amountOut) {
        IERC20(tokenIn).safeApprove(vault, 0);
        IERC20(tokenIn).safeApprove(vault, amountIn);

        IBalancerVault.SingleSwap memory singleSwap = IBalancerVault.SingleSwap({
            poolId:   poolId,
            kind:     IBalancerVault.SwapKind.GIVEN_IN,
            assetIn:  tokenIn,
            assetOut: tokenOut,
            amount:   amountIn,
            userData: ""
        });

        IBalancerVault.FundManagement memory funds = IBalancerVault.FundManagement({
            sender:              address(this),
            fromInternalBalance: false,
            recipient:           payable(address(this)),
            toInternalBalance:   false
        });

        amountOut = IBalancerVault(vault).swap(
            singleSwap,
            funds,
            0,              // limit: accept any amount out
            block.timestamp + 300
        );
    }

    /// @notice Algebra swap callback - called by pool during swap to request input tokens
    function algebraSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external {
        // Only the pool we just called may invoke this callback.
        require(
            _pendingAlgebraPool != address(0) && msg.sender == _pendingAlgebraPool,
            "AlgCB: not pool"
        );
        // Decode only tokenIn from the callback data; use actual pool-computed delta
        (address tokenIn, ) = abi.decode(data, (address, uint256));
        // The pool tells us exactly how much it needs via the positive delta
        uint256 actualAmount = uint256(amount0Delta > 0 ? amount0Delta : amount1Delta);
        // Transfer the exact required amount to the pool
        IERC20(tokenIn).safeTransfer(msg.sender, actualAmount);
    }

    function algebraSwapDirect(
        address poolAddr,
        address tokenIn,
        address tokenOut,
        uint256 amountIn
    ) external returns (uint256 amountOut) {
        require(msg.sender == owner, "not owner");
        IAlgebraPool pool = IAlgebraPool(poolAddr);
        address token0 = pool.token0();
        bool zeroToOne = (tokenIn == token0);

        uint160 limit = zeroToOne
            ? 4295128740                                                    // MIN_SQRT_RATIO + 1
            : 1461446703485210103287273052203988822378723970341;            // MAX_SQRT_RATIO - 1

        bytes memory cbData = abi.encode(tokenIn, amountIn);

        (int256 delta0, int256 delta1) = pool.swap(
            address(this),
            zeroToOne,
            int256(amountIn),
            limit,
            cbData
        );

        amountOut = uint256(zeroToOne ? -delta1 : -delta0);
    }

    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external override returns (bool) {
        // Aave V3 flashLoanSimple callback — verify both the initiator and caller.
        require(initiator == address(this), "invalid initiator");
        require(aavePool != address(0) && msg.sender == aavePool, "not aave pool");
        require(allowedFlashAssets[asset], "asset not allowed");

        uint256 repayAmount = amount + premium;

        if (_aaveLiquidationPending) {
            // ── Liquidation path ─────────────────────────────────────────────
            // Clear guards before any external interaction.
            _aaveLiquidationPending  = false;
            address swapVault        = _pendingAaveLiqSwapVault;
            _pendingAaveLiqSwapVault = address(0);

            LiquidationParams memory p = abi.decode(params, (LiquidationParams));
            _executeLiquidationCore(swapVault, p);

            // Repay AAVE: principal + premium (premium = amount × 0.0005 = 0.05%).
            IERC20(asset).safeApprove(msg.sender, 0);
            IERC20(asset).safeApprove(msg.sender, repayAmount);
        } else {
            // ── Arb path: decode SwapParams and execute ──────────────────────
            // params = abi.encode(SwapParams arb) passed from executeArbitrage().
            SwapParams memory arb = abi.decode(params, (SwapParams));
            uint256 amountOut = _executeSwapPath(arb, asset, amount);
            require(amountOut >= repayAmount, "arb: repay insufficient");

            IERC20(asset).safeApprove(msg.sender, 0);
            IERC20(asset).safeApprove(msg.sender, repayAmount);
        }
        return true;
    }

    function uniswapV3FlashCallback(
        uint256 fee0,
        uint256 fee1,
        bytes calldata data
    ) external override {
        (SwapParams memory arb, address flashPool, address startToken) =
            abi.decode(data, (SwapParams, address, address));

        require(msg.sender == flashPool, "not flash pool");
        require(
            flashPool == uniV3FlashPool || flashPool == pancakeV3FlashPool,
            "unauthorized flash pool"
        );

        uint256 amountOut = _executeSwapPath(arb, startToken, arb.amountIn);
        uint256 repayAmount = arb.amountIn + fee0 + fee1;
        require(amountOut >= repayAmount, "arb: repay insufficient");

        IERC20(startToken).safeTransfer(flashPool, repayAmount);
    }

    function uniswapV2Call(
        address sender,
        uint amount0,
        uint amount1,
        bytes memory data
    ) external override {
        address loanPool;
        address tokenIn;
        uint256 feeBps;

        assembly {
            // Calldata layout (bytes memory data):
            //   offset+0x00 = amountIn   (slot 0)
            //   offset+0x20 = useLoan    (slot 1)
            //   offset+0x40 = loanPool   (slot 2)
            //   offset+0x60 = minOut     (slot 3)
            //   offset+0x80 = hop0.router   (slot 4)
            //   offset+0xa0 = hop0.tokenIn  (slot 5)  ← token we borrowed
            //   offset+0xc0 = hop0.tokenOut (slot 6)
            //   offset+0xe0 = hop0.poolType (slot 7)
            //   offset+0x100 = hop0.fee     (slot 8)  ← loan pool fee in bps
            let offset := add(data, 0x20)
            loanPool := mload(add(offset, 0x40))
            tokenIn  := mload(add(offset, 0xa0))
            feeBps   := mload(add(offset, 0x100))
        }

        require(msg.sender == loanPool, "not loanPool");
        require(sender == address(this), "not sender");

        // we don't need any amountOut checks for this
        // because if we can't pay back the loan, our function simply reverts
        _execute(data);

        uint amountIn = amount0 == 0 ? amount1 : amount0;
        // Dynamic V2 fee: repayFee = amountIn * feeBps / (10000 - feeBps) rounded up
        // e.g. feeBps=71 (Blackhole) → repayFee = amountIn * 71 / 9929 + 1
        //      feeBps=300 (std V2)   → repayFee = amountIn * 300 / 9700 + 1
        uint repayFee = (amountIn * feeBps) / (10000 - feeBps) + 1;
        uint repayAmount = amountIn + repayFee;

        // Repay principal + fee using safeTransfer (supports non-standard tokens).
        IERC20(tokenIn).safeTransfer(loanPool, repayAmount);
    }

    fallback() external payable {
        uint amountIn;
        uint useLoan;
        address loanPool;

        address _owner = owner;

        assembly {
            // only the owner can call fallback
            if iszero(eq(caller(), _owner)) {
                revert(0, 0)
            }

            amountIn := calldataload(0x00)
            useLoan := calldataload(0x20)
            loanPool := calldataload(0x40)
        }

        if (useLoan != 0) {
            address tokenBorrow;

            assembly {
                // the first tokenIn is the token we flashloan
                // header: amountIn(0x00), useLoan(0x20), loanPool(0x40), minOut(0x60)
                // hop0.router = 0x80, hop0.tokenIn = 0xa0
                tokenBorrow := calldataload(0xa0)
            }

            if (useLoan == 1) {
                // Balancer Flashloan
                IERC20[] memory tokens = new IERC20[](1);
                tokens[0] = IERC20(tokenBorrow);

                uint[] memory amounts = new uint[](1);
                amounts[0] = amountIn;

                IBalancerVault(loanPool).flashLoan(
                    IFlashLoanRecipient(address(this)),
                    tokens,
                    amounts,
                    msg.data
                );
            } else if (useLoan == 2) {
                // Uniswap V2 Flashswap
                IUniswapV2Pair pool = IUniswapV2Pair(loanPool);
                address token0 = pool.token0();
                if (tokenBorrow == token0) {
                    pool.swap(amountIn, 0, address(this), msg.data);
                } else {
                    pool.swap(0, amountIn, address(this), msg.data);
                }
            } else if (useLoan == 3) {
                // Aave V3 Flashloan (flashLoanSimple)
                require(allowedFlashAssets[tokenBorrow], "asset not allowed");
                IAaveV3Pool(loanPool).flashLoanSimple(
                    address(this),
                    tokenBorrow,
                    amountIn,
                    msg.data,
                    0
                );
            }
        } else {
            // perform swaps without flashloan
            _execute(msg.data);
        }
    }
}

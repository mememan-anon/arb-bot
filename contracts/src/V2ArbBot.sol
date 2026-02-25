// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import "openzeppelin-contracts/token/ERC20/IERC20.sol";
import "openzeppelin-contracts/token/ERC20/utils/SafeERC20.sol";

import "./interface/IWETH.sol";
import "./interface/IUniswapV2.sol";
import "./interface/IBalancer.sol";
import "./interface/IAaveV3.sol";
import "./interface/IAlgebraPool.sol";
import "./interface/IRamsesV3Pool.sol";
import "./interface/ILBPair.sol";

/// @dev Minimal extension of IERC20 to access decimals() for stable AMM math.
interface IERC20Extended is IERC20 {
    function decimals() external view returns (uint8);
}

contract V2ArbBot is IFlashLoanRecipient, IUniswapV2Callee, IFlashLoanSimpleReceiver, IRamsesV3SwapCallback {
    // can perform flashloan, multihop swaps in Uniswap V2 variant pools
    using SafeERC20 for IERC20;

    address public immutable owner;
    IWETH public immutable mainCurrency;
    mapping(address => bool) public allowedFlashAssets;

    /// @dev Trusted Aave V3 pool address — set by owner, checked in executeOperation.
    address public aavePool;

    /// @dev Transient auth slots for CL swap callbacks — set to the pool being swapped
    ///      before pool.swap() and cleared immediately after.  Prevents any EOA from
    ///      calling the callbacks and draining contract tokens.
    address private _pendingAlgebraPool;
    address private _pendingRamsesPool;

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
        uint amountIn
    ) internal returns (uint amountOut) {
        // Arm callback auth before calling pool.swap(); disarm after.
        _pendingAlgebraPool = poolAddr;
        IAlgebraPool pool = IAlgebraPool(poolAddr);
        bool zeroToOne = (tokenIn == pool.token0());

        uint160 limit = zeroToOne
            ? 4295128740                                                    // MIN_SQRT_RATIO + 1 (Algebra MIN_SQRT_RATIO = 4295128739)
            : 1461446703485210103287273052203988822378723970341;            // MAX_SQRT_RATIO - 1

        (int256 delta0, int256 delta1) = pool.swap(
            address(this),
            zeroToOne,
            int256(amountIn),
            limit,
            abi.encode(tokenIn, amountIn)
        );

        _pendingAlgebraPool = address(0);
        amountOut = uint256(zeroToOne ? -delta1 : -delta0);
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

    // ── Pharaoh CL / RamsesV3 (Uniswap V3 fork) ─────────────────────────────
    function _swapRamsesV3Pool(
        address poolAddr,
        address tokenIn,
        uint    amountIn
    ) internal returns (uint amountOut) {
        // Arm callback auth before calling pool.swap(); disarm after.
        _pendingRamsesPool = poolAddr;
        IRamsesV3Pool pool = IRamsesV3Pool(poolAddr);
        bool zeroForOne = (tokenIn == pool.token0());

        // Standard UniV3 price limits
        uint160 limit = zeroForOne
            ? 4295128740                                          // TickMath.MIN_SQRT_RATIO + 1
            : 1461446703485210103287273052203988822378723970341;  // TickMath.MAX_SQRT_RATIO - 1

        (int256 delta0, int256 delta1) = pool.swap(
            address(this),
            zeroForOne,
            int256(amountIn),
            limit,
            abi.encode(tokenIn, amountIn)
        );

        _pendingRamsesPool = address(0);
        amountOut = uint256(zeroForOne ? -delta1 : -delta0);
    }

    /// @notice RamsesV3 swap callback — called by the pool to pull input tokens.
    /// Named `uniswapV3SwapCallback` because RamsesV3 retains the Uniswap V3 ABI.
    function uniswapV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external override {
        // Only the pool we just called may invoke this callback.
        require(
            _pendingRamsesPool != address(0) && msg.sender == _pendingRamsesPool,
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
                amountOut = _swapAlgebraPool(router, tokenIn, amountOut);
            } else if (poolType == 2) {
                // Solidly stable pair (x³y + xy³ = k) — same swap() ABI but different math.
                // router field holds pair address; feeBps carries the pool fee.
                amountOut = _swapSolidlyStablePair(router, tokenIn, tokenOut, amountOut, feeBps);
            } else if (poolType == 3) {
                // RamsesV3 / Pharaoh CL — Uniswap V3 fork.
                // router field holds pool address; fee handled internally by the pool.
                amountOut = _swapRamsesV3Pool(router, tokenIn, amountOut);
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

    function receiveFlashLoan(
        IERC20[] memory tokens,
        uint[] memory amounts,
        uint[] memory feeAmounts,
        bytes memory data
    ) external override {
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

        // Execute swaps using same calldata format as fallback
        _execute(params);

        uint256 repayAmount = amount + premium;
        // Approve the Aave pool (msg.sender) to pull the repayment.
        // Reset allowance to 0 first to satisfy USDT-style tokens.
        IERC20(asset).safeApprove(msg.sender, 0);
        IERC20(asset).safeApprove(msg.sender, repayAmount);
        return true;
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

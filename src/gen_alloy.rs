/// Alloy sol! bindings for on-chain contracts used by the REVM pipeline.
///
/// FlashQuoter: injected into REVM at 0x1000 for local swap simulation.
/// FlashSwap: on-chain execution contract for submitting arbs.
/// ERC20/V2Pair/V3Pool: ABI fragments for state reads.

use alloy::sol;

/// Deployed bytecode for the FlashQuoter contract (compiled from
/// contracts/src/FlashQuoter.sol). Loaded at compile time via include_str!.
///
/// Regenerate after changing the Solidity source:
///   cd contracts && forge build --force
///   # then copy deployedBytecode.object from out/FlashQuoter.sol/FlashQuoter.json
pub const FLASH_QUOTER_DEPLOYED_BYTECODE: &str =
    include_str!("flash_quoter_bytecode.hex");

// ── FlashQuoter — deployed into REVM at address 0x1000 ──────────────────────
// Used for local REVM swap simulation: quoteArbitrage returns per-hop amountsOut.
sol! {
    #[derive(Debug)]
    #[sol(rpc)]
    contract FlashQuoter {
        struct SwapParams {
            address[] pools;
            uint8[] poolVersions;
            uint32[] fees;
            uint256 amountIn;
            address startToken;
        }

        /// Legacy single-output quote.
        function getAmountOut(SwapParams calldata params) external returns (uint256 amountOut);

        /// Full per-hop quote — returns amount at each hop.
        function quoteArbitrage(SwapParams calldata params) external returns (uint256[] memory amounts);
    }
}

// ── FlashSwap — on-chain arb execution contract ─────────────────────────────
// SwapParams layout matches FlashQuoter so conversion is a struct copy.
// poolVersions: 0=V2/PCS-V2, 1=UniV3/PCS-V3, 2=AlgebraV1/Thena-Fusion, 3=SolidlyStable
// fees: per-hop fee in basis-points (V2/SolidlyStable only; V3/Algebra ignore).
// startToken: explicit first token; avoids ambiguity in 2-hop cycles where both
//             pools contain the same pair and direction cannot be inferred.
sol! {
    #[derive(Debug)]
    #[sol(rpc)]
    contract FlashSwap {
        struct SwapParams {
            address[] pools;
            uint8[] poolVersions;
            uint32[] fees;
            uint256 amountIn;
            address startToken;
            uint8 flashLoanProvider;
        }

        function executeArbitrage(SwapParams calldata arb) external;
    }
}

// ── ERC20 — approve / balanceOf for state_db warm-up ─────────────────────────
sol!(
    #[sol(rpc)]
    contract ERC20Token {
        function approve(address spender, uint256 amount) external returns (bool success);
        function balanceOf(address account) external view returns (uint256);
    }
);

sol! {
    /// Minimal ERC20 interface alias (backward compat).
    #[derive(Debug)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function decimals() external view returns (uint8);
        function transfer(address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);
    }

    /// UniswapV2 pair — reserves and token addresses.
    #[derive(Debug)]
    interface IUniswapV2Pair {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }

    /// UniswapV3 pool — state reads for the calc module.
    #[derive(Debug)]
    interface IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function ticks(int24 tick) external view returns (
            uint128 liquidityGross,
            int128 liquidityNet,
            uint256 feeGrowthOutside0X128,
            uint256 feeGrowthOutside1X128,
            int56 tickCumulativeOutside,
            uint160 secondsPerLiquidityOutsideX128,
            uint32 secondsOutside,
            bool initialized
        );
        function tickBitmap(int16 wordPosition) external view returns (uint256);
    }

    /// Aave V3 flash loan interface.
    #[derive(Debug)]
    interface IAaveV3Pool {
        function flashLoanSimple(
            address receiverAddress,
            address asset,
            uint256 amount,
            bytes calldata params,
            uint16 referralCode
        ) external;
    }
}

// ── DEX Router ABIs — used by the startup REVM swap filter ───────────────────
sol! {
    /// UniswapV2-style router (used by all V2 forks on Base).
    interface IV2Router {
        function swapExactTokensForTokens(
            uint amountIn,
            uint amountOutMin,
            address[] calldata path,
            address to,
            uint deadline
        ) external returns (uint[] memory amounts);
    }

    /// UniswapV3-style router without deadline field (UniswapV3, AlienBaseV3, PancakeSwapV3, DackieSwapV3).
    interface IV3Router {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        function exactInputSingle(ExactInputSingleParams calldata params) external payable returns (uint256 amountOut);
    }

    /// UniswapV3-style router WITH deadline field (SushiSwapV3, BaseSwapV3, SwapBasedV3).
    interface IV3RouterDeadline {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        function exactInputSingle(ExactInputSingleParams calldata params) external payable returns (uint256 amountOut);
    }

    /// Aerodrome/Velodrome router (V2-compatible with Route struct).
    /// 4-field Route includes factory address (Aerodrome on Base).
    interface IAerodromeRouter {
        struct Route {
            address from;
            address to;
            bool stable;
            address factory;
        }
        function swapExactTokensForTokens(
            uint amountIn,
            uint amountOutMin,
            Route[] calldata routes,
            address to,
            uint deadline
        ) external returns (uint[] memory amounts);
    }

    /// Original Solidly V2 router (3-field Route without factory).
    /// Used by Thena on BSC and other Solidly V2 forks that predate Aerodrome.
    interface ISolidlyRouter {
        struct Route {
            address from;
            address to;
            bool stable;
        }
        function swapExactTokensForTokens(
            uint amountIn,
            uint amountOutMin,
            Route[] calldata routes,
            address to,
            uint deadline
        ) external returns (uint[] memory amounts);
    }

    /// Slipstream (Aerodrome CL) router — tick-spacing based, with deadline.
    interface ISlipstreamRouter {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            int24 tickSpacing;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }
        function exactInputSingle(ExactInputSingleParams calldata params) external payable returns (uint256 amountOut);
    }
}

// ── Conversions between FlashQuoter::SwapParams ↔ FlashSwap::SwapParams ──────
impl From<FlashQuoter::SwapParams> for FlashSwap::SwapParams {
    fn from(params: FlashQuoter::SwapParams) -> Self {
        FlashSwap::SwapParams {
            pools: params.pools,
            poolVersions: params.poolVersions,
            fees: params.fees,
            amountIn: params.amountIn,
            startToken: params.startToken,
            flashLoanProvider: 255,
        }
    }
}

impl From<FlashSwap::SwapParams> for FlashQuoter::SwapParams {
    fn from(params: FlashSwap::SwapParams) -> Self {
        FlashQuoter::SwapParams {
            pools: params.pools,
            poolVersions: params.poolVersions,
            fees: params.fees,
            amountIn: params.amountIn,
            startToken: params.startToken,
        }
    }
}

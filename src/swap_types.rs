/// Swap path and step types for the pipeline architecture.
///
/// Ported from BaseBuster's swap.rs. These are the alloy-native path types used
/// by the REVM-based calculator, estimator, searcher, and simulator.

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};
use std::hash::Hash;

use crate::gen_alloy::FlashQuoter;

/// Protocol type enum — mirrors the pool types supported by the on-chain contract.
///
/// Maps to the `poolVersions` byte in the FlashSwap / FlashQuoter contracts:
///   0 = V2-style (constant product)  — UniswapV2, PancakeSwapV2, Solidly volatile
///   1 = UniswapV3-style (V3 CL)      — UniV3, PancakeSwapV3, SushiSwapV3, Slipstream
///   2 = Algebra CL (Thena Fusion)    — algebraSwapCallback instead of uniswapV3SwapCallback
///   3 = Solidly stable AMM           — x³y+xy³=k, same swap() ABI as V2
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PoolProtocol {
    UniswapV2,
    SushiSwapV2,
    PancakeSwapV2,
    BaseSwapV2,
    AlienBaseV2,
    Aerodrome,
    UniswapV3,
    SushiSwapV3,
    BaseSwapV3,
    PancakeSwapV3,
    AlienBaseV3,
    Slipstream,
    /// Algebra V1.9 concentrated-liquidity (Thena Fusion on BSC, Quickswap on Polygon, etc.)
    /// Uses `algebraSwapCallback` instead of `uniswapV3SwapCallback`.
    AlgebraV1,
    /// Balancer V2 weighted pool
    BalancerV2,
    /// Curve two-token crypto pool (CurveTwoCrypto)
    CurveTwoCrypto,
    /// Curve three-token crypto pool (CurveTriCrypto)
    CurveTriCrypto,
    /// Maverick V2 bin-based CL pool
    MaverickV2,
}

impl PoolProtocol {
    /// Is this a V3-style concentrated liquidity pool?
    #[inline]
    pub fn is_v3(&self) -> bool {
        matches!(
            self,
            PoolProtocol::UniswapV3
                | PoolProtocol::SushiSwapV3
                | PoolProtocol::BaseSwapV3
                | PoolProtocol::PancakeSwapV3
                | PoolProtocol::AlienBaseV3
                | PoolProtocol::Slipstream
                | PoolProtocol::AlgebraV1
        )
    }

    /// Is this a V2-style constant-product pool?
    #[inline]
    pub fn is_v2(&self) -> bool {
        matches!(
            self,
            PoolProtocol::UniswapV2
                | PoolProtocol::SushiSwapV2
                | PoolProtocol::PancakeSwapV2
                | PoolProtocol::BaseSwapV2
                | PoolProtocol::AlienBaseV2
        )
    }

    /// Is this a Curve pool (any variant)?
    #[inline]
    pub fn is_curve(&self) -> bool {
        matches!(self, PoolProtocol::CurveTwoCrypto | PoolProtocol::CurveTriCrypto)
    }

    /// Returns the index count expected for this Curve pool (2 or 3).
    #[inline]
    pub fn curve_n_tokens(&self) -> usize {
        match self {
            PoolProtocol::CurveTriCrypto => 3,
            _ => 2,
        }
    }

    /// Get the V2 fee numerator (out of 10000) for constant-product AMMs.
    /// e.g. UniswapV2 = 9970 (0.3% fee), PancakeSwapV2 = 9975 (0.25% fee).
    #[inline]
    pub fn v2_fee_factor(&self) -> u64 {
        match self {
            PoolProtocol::UniswapV2 | PoolProtocol::SushiSwapV2 => 9970,
            PoolProtocol::PancakeSwapV2 | PoolProtocol::BaseSwapV2 => 9975,
            PoolProtocol::AlienBaseV2 => 9984,
            _ => 9970, // default
        }
    }

    /// Convert to the `poolVersions` byte used by FlashSwap / FlashQuoter contracts.
    ///
    /// Encoding:
    ///   0 = V2-style constant-product (UniV2, PCS V2, Solidly volatile, Aerodrome)
    ///   1 = UniswapV3-style CL        (UniV3, PCS V3, SushiV3, Slipstream, AlienBaseV3)
    ///   2 = Algebra V1.9 CL           (Thena Fusion — uses algebraSwapCallback)
    ///   3 = Solidly stable AMM        (x³y+xy³=k, same swap() ABI as V2)
    #[inline]
    pub fn to_quoter_version(&self) -> u8 {
        match self {
            // V3-style CL with uniswapV3SwapCallback
            PoolProtocol::UniswapV3
            | PoolProtocol::SushiSwapV3
            | PoolProtocol::BaseSwapV3
            | PoolProtocol::PancakeSwapV3
            | PoolProtocol::AlienBaseV3
            | PoolProtocol::Slipstream => 1,
            // Algebra V1.9 CL (algebraSwapCallback)
            PoolProtocol::AlgebraV1 => 2,
            // Everything else is treated as V2-style (constant product, push model)
            _ => 0,
        }
    }
}

/// A single swap step within a multi-hop path.
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct SwapStep {
    pub pool_address: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub protocol: PoolProtocol,
    pub fee: u32,
}

/// A complete multi-hop swap path with a precomputed hash for dedup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SwapPath {
    pub steps: Vec<SwapStep>,
    pub hash: u64,
}

impl SwapPath {
    /// Build a new SwapPath and compute its hash.
    pub fn new(steps: Vec<SwapStep>) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::Hasher;
        let mut hasher = DefaultHasher::new();
        for step in &steps {
            step.hash(&mut hasher);
        }
        Self {
            steps,
            hash: hasher.finish(),
        }
    }

    /// Number of hops in this path.
    #[inline]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Does this path contain a specific pool?
    pub fn contains_pool(&self, pool: &Address) -> bool {
        self.steps.iter().any(|s| s.pool_address == *pool)
    }
}

/// Convert from SwapPath → FlashQuoter::SwapParams for REVM quoter.
impl SwapPath {
    pub fn to_quoter_params(&self, amount_in: alloy::primitives::U256) -> FlashQuoter::SwapParams {
        let pools: Vec<alloy::primitives::Address> =
            self.steps.iter().map(|s| s.pool_address).collect();
        let pool_versions: Vec<u8> =
            self.steps.iter().map(|s| s.protocol.to_quoter_version()).collect();
        // fees[i] = per-hop fee in basis-points.
        //   V2 pools: loaded from CSV (e.g. 25 = PancakeSwap V2 0.25%, 30 = UniV2 0.30%).
        //   V3/Algebra pools: fee is handled internally by the pool; the contract ignores
        //   this field for versions 1 and 2, but we still populate it for completeness.
        let fees: Vec<u32> = self.steps.iter().map(|s| s.fee).collect();
        // startToken: the token being sold in hop 0 (the flash-loaned token).
        // Explicitly set so the contract can Aave-flash the right asset even for
        // 2-hop cycles where both pools contain the same pair.
        let start_token = self.steps.first().map(|s| s.token_in)
            .unwrap_or(alloy::primitives::Address::ZERO);
        FlashQuoter::SwapParams {
            pools,
            poolVersions: pool_versions,
            fees,
            amountIn: amount_in,
            startToken: start_token,
        }
    }
}

use anyhow::{anyhow, Result};
use ethers::types::{Address, H160, U256};
use log::{info, warn};
use std::str::FromStr;

use crate::bundler::{Flashloan, PathParam};
use crate::config::{Config, TokenConfig};
use crate::paths::ArbPath;

fn wei_to_gwei(wei: U256) -> f64 {
    wei.as_u128() as f64 / 1e9
}

#[derive(Debug, Clone)]
pub struct ExecutionParams {
    pub path: ArbPath,
    pub amount_in: U256,
    pub expected_profit: U256,
    pub v2_router: H160,
    pub algebra_router: Option<H160>,
    pub start_token: TokenConfig,
    pub base_fee: U256,
    /// USD price of one whole start token (e.g. 1907.0 for WETH). 0.0 if unknown.
    pub token_price_usd: f64,
}

pub struct Executor {
    config: Config,
}

impl Executor {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Check if the profit meets the minimum threshold.
    /// `token_price_usd` is the USD price of one whole start token (e.g. 3000.0 for WETH).
    /// Pass `0.0` if the price is unknown — the check will fall back to raw token units.
    pub fn check_profit_threshold(&self, profit_raw: U256, start_token: &TokenConfig, token_price_usd: f64) -> bool {
        if !self.config.execution.enabled {
            return false;
        }

        // Convert raw profit to whole token units.
        let profit_in_token = (profit_raw.as_u128() as f64)
            / ((10_u128.pow(start_token.decimals as u32)) as f64);

        // Convert to USD.  When token_price_usd == 0.0 (unknown price), fall back to
        // treating token units as USD — correct for stablecoins, conservative otherwise.
        let profit_usd = if token_price_usd > 0.0 {
            profit_in_token * token_price_usd
        } else {
            profit_in_token
        };

        let threshold = self.config.execution.min_profit_threshold;

        if profit_usd < threshold {
            info!(
                "Profit ${:.4} below threshold ${:.4}, skipping execution",
                profit_usd, threshold
            );
            return false;
        }

        info!("Profit ${:.4} exceeds threshold ${:.4}", profit_usd, threshold);
        true
    }

    /// Determine which flash loan provider to use
    pub fn get_flashloan_provider(&self) -> Result<(Flashloan, Address)> {
        let provider_name = &self.config.execution.default_flashloan_provider;

        match provider_name.as_str() {
            "AaveV3" => {
                let aave_cfg = self
                    .config
                    .aave_v3
                    .as_ref()
                    .ok_or_else(|| anyhow!("Aave V3 config missing"))?;
                let pool_address = Address::from_str(&aave_cfg.pool)?;
                Ok((Flashloan::AaveV3, pool_address))
            }
            "Balancer" => {
                let vault_str = self
                    .config
                    .execution
                    .balancer_vault
                    .as_deref()
                    .ok_or_else(|| anyhow!("balancer_vault not set in [execution] config"))?;
                let vault = Address::from_str(vault_str)
                    .map_err(|e| anyhow!("invalid balancer_vault address: {e}"))?;
                Ok((Flashloan::Balancer, vault))
            }
            "UniswapV2" => {
                // For UniswapV2, we need a specific pool address
                // This should be the first pool in the path
                Err(anyhow!("UniswapV2 flash swap requires specific pool - not implemented for default"))
            }
            _ => Err(anyhow!("Unknown flashloan provider: {}", provider_name)),
        }
    }

    /// Calculate dynamic gas fees based on current network conditions
    pub fn calculate_gas_fees(&self, current_base_fee: U256) -> (U256, U256) {
        let max_priority_fee_gwei = self.config.execution.max_priority_fee_gwei;
        let max_base_fee_multiplier = 2; // Allow base fee to go up to 2x current

        // Priority fee (tip to validators)
        // max_priority_fee_gwei is f64 (e.g. 0.01 gwei on Base L2).
        // Convert: gwei * 1e9 = wei.
        let max_priority_fee = U256::from((max_priority_fee_gwei * 1e9) as u128);

        // Max fee = (base_fee * multiplier) + priority_fee
        let max_fee_per_gas = (current_base_fee * max_base_fee_multiplier) + max_priority_fee;

        // Cap the max fee
        let max_base_fee_cap = U256::from((self.config.execution.max_base_fee_gwei * 1e9) as u128);
        let max_fee_per_gas = if max_fee_per_gas > max_base_fee_cap {
            warn!(
                "Calculated max fee {} exceeds cap {}, using cap",
                max_fee_per_gas, max_base_fee_cap
            );
            max_base_fee_cap
        } else {
            max_fee_per_gas
        };

        // Floor: max_fee_per_gas must be >= current_base_fee + priority_fee,
        // otherwise all nodes will reject the transaction outright.
        let fee_floor = current_base_fee + max_priority_fee;
        let max_fee_per_gas = if max_fee_per_gas < fee_floor {
            warn!(
                "max_fee_per_gas {} below floor {} (base_fee + priority), raising to floor",
                max_fee_per_gas, fee_floor
            );
            fee_floor
        } else {
            max_fee_per_gas
        };

        info!(
            "Gas fees: priority={:.6} gwei ({} wei), max_fee={:.6} gwei ({} wei)",
            wei_to_gwei(max_priority_fee),
            max_priority_fee,
            wei_to_gwei(max_fee_per_gas),
            max_fee_per_gas
        );

        (max_priority_fee, max_fee_per_gas)
    }

    /// Build transaction parameters for execution
    pub fn build_execution_tx(
        &self,
        params: &ExecutionParams,
    ) -> Result<ExecutionPlan> {
        // Convert ArbPath to PathParams
        let direct_pair = self.config.dexes.v2.as_ref().map(|v| v.direct_pair).unwrap_or(false);
        let path_params = params
            .path
            .to_path_params(params.v2_router, params.algebra_router, direct_pair);

        // Get flashloan provider
        let (flashloan_type, loan_pool) = self.get_flashloan_provider()?;

        // Calculate gas fees
        let (max_priority_fee, max_fee_per_gas) =
            self.calculate_gas_fees(params.base_fee);

        Ok(ExecutionPlan {
            path_params,
            amount_in: params.amount_in,
            flashloan_type,
            loan_pool,
            max_priority_fee,
            max_fee_per_gas,
            expected_profit: params.expected_profit,
        })
    }

    /// Log execution details for monitoring
    pub fn log_execution(&self, params: &ExecutionParams, plan: &ExecutionPlan) {
        let profit_in_token = (params.expected_profit.as_u128() as f64)
            / ((10_u128.pow(params.start_token.decimals as u32)) as f64);

        let profit_usd = if params.token_price_usd > 0.0 {
            profit_in_token * params.token_price_usd
        } else {
            profit_in_token
        };

        info!("=== EXECUTION PLAN ===");
        info!("Amount In: {} {}",
            params.amount_in.as_u128() as f64 / (10_u128.pow(params.start_token.decimals as u32)) as f64,
            params.start_token.symbol
        );
        info!("Expected Profit: {:.6} {} (${:.2})",
            profit_in_token,
            params.start_token.symbol,
            profit_usd
        );
        info!("Flash Loan: {:?} from {:?}", plan.flashloan_type, plan.loan_pool);
        info!("Path Hops: {}", plan.path_params.len());
        info!("Max Priority Fee: {:.6} gwei", plan.max_priority_fee.as_u128() as f64 / 1e9);
        info!("Max Fee Per Gas: {:.6} gwei", plan.max_fee_per_gas.as_u128() as f64 / 1e9);
        info!("======================");
    }
}

#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub path_params: Vec<PathParam>,
    pub amount_in: U256,
    pub flashloan_type: Flashloan,
    pub loan_pool: Address,
    pub max_priority_fee: U256,
    pub max_fee_per_gas: U256,
    pub expected_profit: U256,
}

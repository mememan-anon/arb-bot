use anyhow::Result;
use ethers::{
    self,
    abi::{decode, ParamType, Token},
    providers::{Middleware, Provider, Ws},
    types::{Filter, H160, H256, U256, U64},
};
use fern::colors::{Color, ColoredLevelConfig};
use log::LevelFilter;
use std::{collections::{HashMap, HashSet}, str::FromStr, sync::Arc};

use crate::multi::Reserve;

fn colorize_metrics(input: &str) -> String {
    input
        .split_whitespace()
        .map(|token| {
            if token.starts_with("block=") {
                format!("\x1b[96m{}\x1b[0m", token)
            } else {
                token.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn setup_logger() -> Result<()> {
    let colors = ColoredLevelConfig {
        trace: Color::Cyan,
        debug: Color::Magenta,
        info: Color::Green,
        warn: Color::Red,
        error: Color::BrightRed,
        ..ColoredLevelConfig::new()
    };

    let app_level = match std::env::var("BOT_LOG_LEVEL")
        .unwrap_or_else(|_| "info".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "trace" => LevelFilter::Trace,
        "debug" => LevelFilter::Debug,
        "warn" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        _ => LevelFilter::Info,
    };

    fern::Dispatch::new()
        .format(move |out, message, record| {
            let ts = chrono::Local::now().format("%H:%M:%S%.3f");
            let msg = colorize_metrics(&message.to_string());
            out.finish(format_args!(
                "\x1b[94m[{}]\x1b[0m[{}] {}",
                ts,
                colors.color(record.level()),
                msg
            ))
        })
        .chain(std::io::stdout())
        .level(log::LevelFilter::Error)
        .level_for("rust", app_level)
        .apply()?;

    Ok(())
}

pub fn calculate_next_block_base_fee(
    gas_used: U256,
    gas_limit: U256,
    base_fee_per_gas: U256,
) -> U256 {
    let gas_used = gas_used;

    let mut target_gas_used = gas_limit / 2;
    target_gas_used = if target_gas_used == U256::zero() {
        U256::one()
    } else {
        target_gas_used
    };

    let new_base_fee = {
        if gas_used > target_gas_used {
            base_fee_per_gas
                + ((base_fee_per_gas * (gas_used - target_gas_used)) / target_gas_used)
                    / U256::from(8u64)
        } else {
            base_fee_per_gas
                - ((base_fee_per_gas * (target_gas_used - gas_used)) / target_gas_used)
                    / U256::from(8u64)
        }
    };

    new_base_fee
}

pub async fn get_touched_pool_reserves(
    provider: Arc<Provider<Ws>>,
    block_number: U64,
) -> Result<HashMap<H160, Reserve>> {
    let sync_event = "Sync(uint112,uint112)";
    let event_filter = Filter::new()
        .from_block(block_number)
        .to_block(block_number)
        .event(sync_event);

    let logs = provider.get_logs(&event_filter).await?;

    let mut tx_idx = HashMap::new();
    let mut reserves = HashMap::new();

    for log in &logs {
        let decoded = decode(&[ParamType::Uint(256), ParamType::Uint(256)], &log.data);
        match decoded {
            Ok(data) => {
                let idx = log.transaction_index.unwrap_or_default();
                let prev_tx_idx = tx_idx.get(&log.address);
                let update = (*prev_tx_idx.unwrap_or(&U64::zero())) <= idx;

                if update {
                    let reserve0 = match data[0] {
                        Token::Uint(rs) => rs,
                        _ => U256::zero(),
                    };
                    let reserve1 = match data[1] {
                        Token::Uint(rs) => rs,
                        _ => U256::zero(),
                    };
                    let reserve = Reserve { reserve0, reserve1 };

                    reserves.insert(log.address, reserve);
                    tx_idx.insert(log.address, idx);
                }
            }
            Err(_) => {}
        }
    }

    Ok(reserves)
}

/// Query all Uniswap V3-style Swap events that fired in `block_number`.
///
/// Returns the set of pool addresses (log emitters) that had at least one swap.
/// One `eth_getLogs` call per block — same pattern as `get_touched_pool_reserves`.
///
/// V3 Swap signature: Swap(address,address,int256,int256,uint160,uint128,int24)
pub async fn get_touched_v3cl_pools(
    provider: Arc<Provider<Ws>>,
    block_number: U64,
) -> Result<HashSet<H160>> {
    const SWAP_TOPIC: &str =
        "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";
    let topic = H256::from_str(SWAP_TOPIC)?;
    let filter = Filter::new()
        .from_block(block_number)
        .to_block(block_number)
        .topic0(topic);

    let logs = provider.get_logs(&filter).await?;
    Ok(logs.into_iter().map(|l| l.address).collect())
}

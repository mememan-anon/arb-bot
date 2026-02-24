use anyhow::{Ok, Result};
use ethers::{
    abi,
    providers::{Http, Provider},
    types::{Address, H160, U256},
};
use ethers_contract::{Contract, Multicall};
use log::{info, warn};
use std::{collections::HashMap, str::FromStr, sync::Arc, time::Instant};

use crate::{abi::ABI, pools::Pool};

// Multicall3 is deployed at this address on every EVM chain (including Base).
// ethers-rs only knows a handful of chain IDs by default, so we always
// supply the address explicitly instead of relying on auto-detection.
const MULTICALL3_ADDRESS: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

#[derive(Default, Debug, Clone)]
pub struct Reserve {
    pub reserve0: U256,
    pub reserve1: U256,
}

pub async fn get_uniswap_v2_reserves(
    https_url: String,
    pools: Vec<Pool>,
) -> Result<HashMap<H160, Reserve>> {
    let client = Provider::<Http>::try_from(https_url)?;
    let client = Arc::new(client);

    let abi = ABI::new();
    let multicall_addr: Address = Address::from_str(MULTICALL3_ADDRESS)
        .expect("Multicall3 address is a valid H160");
    let mut multicall = Multicall::new(client.clone(), Some(multicall_addr)).await?;

    for pool in &pools {
        let contract = Contract::<Provider<Http>>::new(
            pool.address,
            abi.uniswap_v2_pair.clone(),
            client.clone(),
        );
        let call = contract.method::<_, (U256, U256, U256)>("getReserves", ())?;
        multicall.add_call(call, false);
    }

    let result = multicall.call_raw().await?;

    let mut reserves = HashMap::new();

    for i in 0..result.len() {
        let pool = &pools[i];
        let token = match result[i].clone().ok() {
            Some(t) => t,
            None => {
                warn!("Multicall getReserves failed for pool {:?}, skipping", pool.address);
                continue;
            }
        };
        match token {
            abi::Token::Tuple(response) => {
                if response.len() < 2 {
                    continue;
                }
                let r0 = match response[0].clone().into_uint() {
                    Some(v) => v,
                    None => continue,
                };
                let r1 = match response[1].clone().into_uint() {
                    Some(v) => v,
                    None => continue,
                };
                let reserve_data = Reserve {
                    reserve0: r0,
                    reserve1: r1,
                };
                reserves.insert(pool.address.clone(), reserve_data);
            }
            _ => {}
        }
    }

    Ok(reserves)
}

pub async fn batch_get_uniswap_v2_reserves(
    https_url: String,
    pools: Vec<Pool>,
) -> HashMap<H160, Reserve> {
    let start_time = Instant::now();

    let pools_cnt = pools.len();
    // Increased from 250 to 1000 for better performance on Avalanche RPC
    let batch = std::cmp::max(1, (pools_cnt + 999) / 1000) as usize;
    let pools_per_batch = ((pools_cnt as f32) / (batch as f32)).ceil() as usize;

    let mut handles = vec![];

    for i in 0..batch {
        let start_idx = i * pools_per_batch;
        let end_idx = std::cmp::min(start_idx + pools_per_batch, pools_cnt);
        let handle = tokio::spawn(get_uniswap_v2_reserves(
            https_url.clone(),
            pools[start_idx..end_idx].to_vec(),
        ));
        handles.push(handle);
    }

    let mut reserves: HashMap<H160, Reserve> = HashMap::new();

    for handle in handles {
        match handle.await {
            std::result::Result::Ok(std::result::Result::Ok(batch)) => reserves.extend(batch),
            std::result::Result::Ok(Err(e)) => warn!("Reserve batch failed: {e}"),
            Err(e) => warn!("Reserve batch task panicked: {e}"),
        }
    }

    info!(
        "Batch reserves call took: {} seconds",
        start_time.elapsed().as_secs()
    );
    reserves
}

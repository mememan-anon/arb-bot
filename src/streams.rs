use ethers::{
    providers::{Provider, Ws},
    types::{Filter, Log, Transaction, U256, U64},
};
use ethers_providers::Middleware;
use log::warn;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
use tokio_stream::StreamExt;

use crate::utils::calculate_next_block_base_fee;

#[derive(Default, Debug, Clone)]
pub struct NewBlock {
    pub block_number: U64,
    pub base_fee: U256,
    pub next_base_fee: U256,
}

#[derive(Debug, Clone)]
pub enum Event {
    Block(NewBlock),
    PendingTx(Transaction),
    Log(Log),
}

pub async fn stream_new_blocks(provider: Arc<Provider<Ws>>, event_sender: Sender<Event>) {
    loop {
        match provider.subscribe_blocks().await {
            Ok(stream) => {
                let mut stream = stream.filter_map(|block| match block.number {
                    Some(number) => Some(NewBlock {
                        block_number: number,
                        base_fee: block.base_fee_per_gas.unwrap_or_default(),
                        next_base_fee: U256::from(calculate_next_block_base_fee(
                            block.gas_used,
                            block.gas_limit,
                            block.base_fee_per_gas.unwrap_or_default(),
                        )),
                    }),
                    None => None,
                });

                while let Some(block) = stream.next().await {
                    match event_sender.send(Event::Block(block)) {
                        Ok(_) => {}
                        Err(_) => {}
                    }
                }
                warn!("Block stream ended unexpectedly, reconnecting...");
            }
            Err(e) => {
                warn!("Failed to subscribe_blocks: {:?}, retrying in 2s...", e);
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }
}

pub async fn stream_pending_transactions(provider: Arc<Provider<Ws>>, event_sender: Sender<Event>) {
    loop {
        match provider.subscribe_pending_txs().await {
            Ok(stream) => {
                let mut stream = stream.transactions_unordered(256).fuse();

                while let Some(result) = stream.next().await {
                    match result {
                        Ok(tx) => match event_sender.send(Event::PendingTx(tx)) {
                            Ok(_) => {}
                            Err(_) => {}
                        },
                        Err(_) => {}
                    };
                }
                warn!("Pending tx stream ended unexpectedly, reconnecting...");
            }
            Err(e) => {
                warn!("Failed to subscribe_pending_txs: {:?}, retrying in 2s...", e);
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }
}

pub async fn stream_uniswap_v2_events(provider: Arc<Provider<Ws>>, event_sender: Sender<Event>) {
    let filter = Filter::new().event("Sync(uint112,uint112)");
    loop {
        match provider.subscribe_logs(&filter).await {
            Ok(mut stream) => {
                while let Some(result) = stream.next().await {
                    match event_sender.send(Event::Log(result)) {
                        Ok(_) => {}
                        Err(_) => {}
                    };
                }
                warn!("V2 event stream ended unexpectedly, reconnecting...");
            }
            Err(e) => {
                warn!("Failed to subscribe_logs: {:?}, retrying in 2s...", e);
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    }
}

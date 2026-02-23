use anyhow::Result;
use ethers::providers::{Provider, Ws};
use log::info;
use std::sync::Arc;
use tokio::sync::broadcast::{self, Sender};
use tokio::task::JoinSet;

use rust::config::Config;
use rust::strategy::event_handler;
use rust::streams::{stream_new_blocks, Event};
use rust::utils::setup_logger;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    setup_logger()?;

    let config = Config::load()?;
    // Start async websocket streams
    let ws = connect_ws_with_fallback(&config).await?;
    let provider = Arc::new(Provider::new(ws));

    let (event_sender, _): (Sender<Event>, _) = broadcast::channel(512);

    let mut set = JoinSet::new();

    set.spawn(stream_new_blocks(provider.clone(), event_sender.clone()));
    // we're not using the mempool data here, but uncomment it to use pending txs
    // set.spawn(stream_pending_transactions(
    //     provider.clone(),
    //     event_sender.clone(),
    // ));
    set.spawn(event_handler(
        provider.clone(),
        event_sender.clone(),
        config,
    ));

    while let Some(res) = set.join_next().await {
        info!("{:?}", res);
    }

    Ok(())
}

async fn connect_ws_with_fallback(config: &Config) -> Result<Ws> {
    let mut urls = Vec::new();
    urls.push(config.chain.wss_url.clone());
    urls.extend(config.chain.wss_urls.clone());

    let mut last_err: Option<anyhow::Error> = None;
    for url in urls {
        match Ws::connect(url.clone()).await {
            Ok(ws) => return Ok(ws),
            Err(e) => {
                last_err = Some(anyhow::anyhow!("WSS connect failed for {url}: {e}"));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No WSS URLs configured")))
}

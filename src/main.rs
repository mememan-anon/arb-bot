use anyhow::Result;
use ethers::providers::{Provider, Ws};
use log::info;
use std::sync::Arc;
use tokio::sync::broadcast::{self, Sender};
use tokio::task::JoinSet;

use rust::config::{Config, LiquidationConfig};
use rust::liquidation::LiquidationUpdate;
use rust::strategy::event_handler;
use rust::streams::{stream_new_blocks, Event};
use rust::utils::setup_logger;

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

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

    // ── Arbitrage sub-system ────────────────────────────────────────────────
    // Set `arbitrage_enabled = false` in the chain config to disable entirely
    // and run the bot in liquidation-only mode.
    if config.arbitrage_enabled {
        set.spawn(stream_new_blocks(provider.clone(), event_sender.clone()));
        // we're not using the mempool data here, but uncomment it to use pending txs
        // set.spawn(stream_pending_transactions(
        //     provider.clone(),
        //     event_sender.clone(),
        // ));
        set.spawn(event_handler(
            provider.clone(),
            event_sender.clone(),
            config.clone(),
        ));
        info!("Arbitrage engine started");
    } else {
        info!("Arbitrage engine DISABLED (arbitrage_enabled = false)");
    }

    // ── Phase 1 + 2: Liquidation Monitor + Health Factor Cache ─────────────────
    // Phase 1 mirrors whistleblower-rs; Phase 2 mirrors vega-rs.
    // monitoring_enabled = true  →  activates Phase 1
    // ui_pool_data_provider set  →  also activates Phase 2 (HF cache + alerts)
    if let Some(aave_cfg) = &config.aave_v3 {
        if aave_cfg.monitoring_enabled {
            let liquidation_wss_url = env_non_empty("LIQUIDATION_WSS_URL")
                .unwrap_or_else(|| config.chain.wss_url.clone());
            let liquidation_https_url = env_non_empty("LIQUIDATIONM_HTTPS_URL")
                .or_else(|| env_non_empty("LIQUIDATION_HTTPS_URL"))
                .unwrap_or_else(|| config.chain.https_url.clone());

            if env_non_empty("LIQUIDATION_WSS_URL").is_some() {
                info!("[liquidation] using LIQUIDATION_WSS_URL override");
            }
            if env_non_empty("LIQUIDATIONM_HTTPS_URL").is_some()
                || env_non_empty("LIQUIDATION_HTTPS_URL").is_some()
            {
                info!("[liquidation] using liquidation-specific HTTPS override");
            }

            let wss_url = liquidation_wss_url;
            let pool_addr = aave_cfg.pool.clone();

            let _ = liquidation_https_url;

            // mpsc channel: Phase 1 + 2.5 → Phase 2
            // 2048-slot channel: absorbs event bursts while HF scans run as background tasks.
            let (liq_tx, liq_rx) = tokio::sync::mpsc::channel::<LiquidationUpdate>(2048);

            // Phase 1: monitor (whistleblower port)
            set.spawn(rust::liquidation::run_monitor(
                wss_url.clone(),
                pool_addr.clone(),
                liq_tx.clone(),
            ));

            // Phase 2.5: Chainlink price trigger
            // Both paths (confirmed AnswerUpdated logs + pending mempool txs) write
            // into the same liq_tx channel so health_factor reacts immediately when
            // oracle prices move, not only when users borrow/repay.
            if !aave_cfg.chainlink_feeds.is_empty() {
                let feeds = aave_cfg.chainlink_feeds.clone();
                let n = feeds.len();
                set.spawn(rust::liquidation::run_price_trigger(
                    wss_url.clone(),
                    feeds,
                    liq_tx.clone(),
                    aave_cfg.chainlink_pending_txs,
                ));
                info!("[liquidation] Phase 2.5 Chainlink price trigger active ({n} feeds)");
            }

            if aave_cfg.offchain_price.enabled {
                set.spawn(rust::liquidation::run_offchain_price(
                    liquidation_https_url.clone(),
                    aave_cfg.offchain_price.clone(),
                    aave_cfg.chainlink_feeds.clone(),
                    liq_tx.clone(),
                ));
                info!("[liquidation] Offchain early-warning trigger active");
            }

            if let Some(ui_addr) = aave_cfg.ui_pool_data_provider.clone() {
                // Phase 2: health factor cache (vega-rs port)
                let pap_addr = aave_cfg.pool_addresses_provider.clone().unwrap_or_default();
                let liq_cfg = config
                    .liquidation
                    .clone()
                    .unwrap_or_else(|| LiquidationConfig::from_execution(&config.execution));

                // broadcast channel: Phase 2 → Phase 3 (executor) + alert subscriber
                let (alert_tx, alert_rx) = broadcast::channel(256);

                set.spawn(rust::liquidation::run_health_factor(
                    wss_url.clone(),
                    pool_addr.clone(),
                    pap_addr.clone(),
                    ui_addr,
                    aave_cfg.borrowers_file.clone(),
                    liq_rx,
                    alert_tx,
                ));

                // Phase 3: executor — subscribes to underwater-user alerts and
                // submits triggerLiquidation txs when a profitable op is found.
                if liq_cfg.enabled {
                    let bot = liq_cfg.balancer_vault.is_some();
                    if bot {
                        let oracle = aave_cfg.oracle.clone().unwrap_or_default();
                        let executor_fut = rust::liquidation::run_executor(
                            wss_url.clone(),
                            pap_addr,
                            aave_cfg.ui_pool_data_provider.clone().unwrap_or_default(),
                            oracle,
                            config.dexes.uniswapv3cl
                                .as_ref()
                                .and_then(|c| c.factory.clone())
                                .unwrap_or_default(),
                            config.start_tokens.first()
                                .map(|t| t.address.clone())
                                .unwrap_or_default(),
                            std::env::var("BOT_ADDRESS").unwrap_or_default(),
                            liq_cfg.balancer_vault.clone().unwrap_or_default(),
                            liq_cfg.morpho.clone(),
                            liq_cfg.aave_pool.clone(),
                            liq_cfg.clone(),
                            alert_rx,
                            // Separate WSS endpoint for TX submission (keeps liquidation
                            // txs out of the public mempool). Set PRIVATE_WSS_URL in .env.
                            std::env::var("PRIVATE_WSS_URL").ok(),
                        );
                        set.spawn(async move {
                            if let Err(e) = executor_fut.await {
                                log::error!("[executor] fatal: {e:#}");
                            }
                        });
                        info!("[liquidation] Phase 3 executor active");
                    } else {
                        info!("[liquidation] executor disabled — set liquidation.balancer_vault in config");
                    }
                } else {
                    // Drain alerts so the channel doesn't stall health_factor.
                    set.spawn(async move {
                        let mut rx = alert_rx;
                        while let Ok(alert) = rx.recv().await {
                            info!(
                                "[health_factor] ALERT user={} hf={} collateral={} trace={}",
                                alert.user,
                                alert.health_factor,
                                alert.total_collateral_base,
                                alert.trace_id,
                            );
                        }
                    });
                }

                info!("[liquidation] Phase 1+2 active (pool={})", pool_addr);
            } else {
                // Phase 2 not yet configured — log raw events
                set.spawn(async move {
                    let mut rx = liq_rx;
                    while let Some(update) = rx.recv().await {
                        info!(
                            "[liquidation] trace={} block={} event={:?}",
                            update.trace_id,
                            update.block_number,
                            update.event_details.event
                        );
                    }
                });
                info!(
                    "[liquidation] Phase 1 active (pool={}) — set aave_v3.ui_pool_data_provider for Phase 2",
                    pool_addr
                );
            }
        }
    }

    // If no tasks were spawned (both sub-systems disabled) just wait for Ctrl-C
    // so the process doesn't exit silently.
    if set.is_empty() {
        info!("No sub-systems enabled — waiting for Ctrl-C");
        tokio::signal::ctrl_c().await?;
        return Ok(());
    }

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

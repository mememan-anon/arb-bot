use std::{collections::HashMap, fs::File, io::{BufRead, BufReader}, time::{SystemTime, UNIX_EPOCH}};

use log::{error, info, warn};
use serde::Deserialize;
use tokio::{sync::mpsc, time::{sleep, Duration, interval}};

use crate::config::{ChainlinkFeed, OffchainAssetConfig, OffchainPriceConfig};

use super::types::{CanonicalPriceEvent, LiquidationUpdate, WhistleblowerEventDetails, WhistleblowerEventType};

#[derive(Deserialize)]
struct PythLatestResponse {
    parsed: Vec<PythParsed>,
}

#[derive(Deserialize)]
struct PythParsed {
    id: String,
    price: PythPrice,
}

#[derive(Deserialize)]
struct PythPrice {
    price: String,
    conf: String,
    expo: i32,
    publish_time: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn pow10_i128(n: u32) -> Option<i128> {
    let mut out: i128 = 1;
    for _ in 0..n {
        out = out.checked_mul(10)?;
    }
    Some(out)
}

fn scale_to_e8(raw: i128, expo: i32) -> Option<i128> {
    let shift = expo + 8;
    if shift >= 0 {
        raw.checked_mul(pow10_i128(shift as u32)?)
    } else {
        let div = pow10_i128((-shift) as u32)?;
        if div == 0 { None } else { Some(raw / div) }
    }
}

fn decode_int256_word(word_hex: &str) -> Option<i128> {
    let unsigned = u128::from_str_radix(word_hex, 16).ok()?;
    if (unsigned >> 127) == 0 {
        i128::try_from(unsigned).ok()
    } else {
        let signed = (unsigned as i128).wrapping_sub(i128::MIN).wrapping_add(i128::MIN);
        Some(signed)
    }
}

fn parse_latest_round_data(result: &str) -> Option<(i128, u64)> {
    let bytes = result.strip_prefix("0x")?;
    if bytes.len() < 64 * 5 {
        return None;
    }
    let answer_word = &bytes[64..128];
    let updated_word = &bytes[192..256];
    let answer = decode_int256_word(answer_word)?;
    let updated = u64::from_str_radix(updated_word, 16).ok()?;
    Some((answer, updated))
}

fn liquid_update(event: &CanonicalPriceEvent) -> LiquidationUpdate {
    let trace = format!("OF{:08x}", (event.received_at_ms & 0xffff_ffff) as u32);
    LiquidationUpdate {
        trace_id: trace,
        block_number: 0,
        enqueued_at_ms: now_ms(),
        event_details: WhistleblowerEventDetails {
            event: WhistleblowerEventType::PriceUpdate,
            args: vec![
                event.asset.to_string(),
                event.source.clone(),
                event.price_e8.to_string(),
                event.confidence_bps.to_string(),
                event.publish_time.to_string(),
            ],
        },
    }
}

async fn run_pyth(
    endpoint: String,
    poll_ms: u64,
    assets: Vec<OffchainAssetConfig>,
    tx: mpsc::Sender<CanonicalPriceEvent>,
) {
    let feed_to_asset: HashMap<String, (String, alloy::primitives::Address)> = assets
        .iter()
        .filter_map(|a| {
            let id = a.pyth_feed_id.clone()?;
            let addr: alloy::primitives::Address = a.asset.parse().ok()?;
            Some((id.to_lowercase(), (a.symbol.clone(), addr)))
        })
        .collect();

    if feed_to_asset.is_empty() {
        return;
    }

    let ids: Vec<String> = feed_to_asset.keys().cloned().collect();
    let client = reqwest::Client::new();

    let mut delay_ms = poll_ms;
    loop {
        let mut url = format!("{}/v2/updates/price/latest?encoding=hex", endpoint.trim_end_matches('/'));
        for id in &ids {
            url.push_str("&ids[]=");
            url.push_str(id);
        }

        match client.get(&url).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    warn!("[offchain_price] pyth http status={} body={}", status, body.chars().take(160).collect::<String>());
                    delay_ms = (delay_ms.saturating_mul(2)).min(5000);
                    sleep(Duration::from_millis(delay_ms)).await;
                    continue;
                }

                match resp.json::<PythLatestResponse>().await {
                    Ok(body) => {
                        delay_ms = poll_ms;
                    for entry in body.parsed {
                        let key = entry.id.to_lowercase();
                        let Some((_sym, asset)) = feed_to_asset.get(&key) else { continue; };
                        let Ok(raw_price) = entry.price.price.parse::<i128>() else { continue; };
                        let Ok(raw_conf) = entry.price.conf.parse::<i128>() else { continue; };
                        let Some(price_e8) = scale_to_e8(raw_price, entry.price.expo) else { continue; };
                        let conf_bps = if raw_price == 0 {
                            u32::MAX
                        } else {
                            ((raw_conf.unsigned_abs() as u128)
                                .saturating_mul(10_000)
                                .checked_div(raw_price.unsigned_abs() as u128)
                                .unwrap_or(u128::MAX)
                                .min(u32::MAX as u128)) as u32
                        };
                        let evt = CanonicalPriceEvent {
                            source: "pyth_hermes".to_string(),
                            asset: *asset,
                            price_e8,
                            confidence_bps: conf_bps,
                            publish_time: entry.price.publish_time,
                            received_at_ms: now_ms(),
                        };
                        let _ = tx.send(evt).await;
                    }
                    }
                    Err(e) => {
                        warn!("[offchain_price] pyth parse error: {e}");
                        delay_ms = (delay_ms.saturating_mul(2)).min(5000);
                    }
                }
            }
            Err(e) => {
                warn!("[offchain_price] pyth request error: {e}");
                delay_ms = (delay_ms.saturating_mul(2)).min(5000);
            }
        }

        sleep(Duration::from_millis(delay_ms)).await;
    }
}

async fn run_chainlink(
    https_rpc_url: String,
    poll_ms: u64,
    assets: Vec<OffchainAssetConfig>,
    chainlink_feeds: Vec<ChainlinkFeed>,
    tx: mpsc::Sender<CanonicalPriceEvent>,
) {
    let mut proxy_by_asset = HashMap::new();
    for f in chainlink_feeds {
        if let Ok(asset) = f.asset.parse::<alloy::primitives::Address>() {
            proxy_by_asset.insert(asset, f.proxy);
        }
    }

    let watched: Vec<(alloy::primitives::Address, String)> = assets
        .into_iter()
        .filter_map(|a| {
            let asset = a.asset.parse::<alloy::primitives::Address>().ok()?;
            let _path = a.chainlink_feed_path.clone()?;
            let proxy = proxy_by_asset.get(&asset)?.clone();
            Some((asset, proxy))
        })
        .collect();

    if watched.is_empty() {
        return;
    }

    let client = reqwest::Client::new();
    loop {
        for (asset, proxy) in &watched {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_call",
                "params": [
                    {
                        "to": proxy,
                        "data": "0xfeaf968c"
                    },
                    "latest"
                ]
            });

            match client.post(&https_rpc_url).json(&body).send().await {
                Ok(resp) => {
                    let Ok(v) = resp.json::<serde_json::Value>().await else { continue; };
                    let Some(result) = v.get("result").and_then(|x| x.as_str()) else { continue; };
                    let Some((answer_e8, updated_at)) = parse_latest_round_data(result) else { continue; };
                    let evt = CanonicalPriceEvent {
                        source: "chainlink_streams".to_string(),
                        asset: *asset,
                        price_e8: answer_e8,
                        confidence_bps: 0,
                        publish_time: updated_at,
                        received_at_ms: now_ms(),
                    };
                    let _ = tx.send(evt).await;
                }
                Err(e) => warn!("[offchain_price] chainlink poll error: {e}"),
            }
        }
        sleep(Duration::from_millis(poll_ms)).await;
    }
}

async fn run_replay(path: String, tx: mpsc::Sender<CanonicalPriceEvent>) {
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            warn!("[offchain_price] replay file open failed ({path}): {e}");
            return;
        }
    };

    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<CanonicalPriceEvent>(&line) {
            Ok(mut evt) => {
                evt.source = "replay".to_string();
                evt.received_at_ms = now_ms();
                let _ = tx.send(evt).await;
            }
            Err(e) => warn!("[offchain_price] replay parse error: {e}"),
        }
    }
}

pub async fn run(
    https_rpc_url: String,
    cfg: OffchainPriceConfig,
    chainlink_feeds: Vec<ChainlinkFeed>,
    liq_tx: mpsc::Sender<LiquidationUpdate>,
) {
    if !cfg.enabled {
        return;
    }

    let (price_tx, mut price_rx) = mpsc::channel::<CanonicalPriceEvent>(2048);

    if let Some(pyth) = cfg.pyth_hermes.clone() {
        tokio::spawn(run_pyth(
            pyth.endpoint,
            pyth.poll_interval_ms,
            cfg.assets.clone(),
            price_tx.clone(),
        ));
    }

    if let Some(chainlink) = cfg.chainlink_streams.clone() {
        let rpc = if https_rpc_url.trim().is_empty() {
            chainlink.endpoint
        } else {
            https_rpc_url.clone()
        };
        tokio::spawn(run_chainlink(
            rpc,
            chainlink.poll_interval_ms,
            cfg.assets.clone(),
            chainlink_feeds,
            price_tx.clone(),
        ));
    }

    if let Some(path) = cfg.replay_file.clone() {
        tokio::spawn(run_replay(path, price_tx.clone()));
    }

    if cfg.require_onchain_confirmation {
        info!("[offchain_price] onchain-confirmation safety is enabled");
    }

    let mut last_emit: HashMap<alloy::primitives::Address, u64> = HashMap::new();
    let mut accepted = 0_u64;
    let mut dropped_stale = 0_u64;
    let mut dropped_conf = 0_u64;
    let mut dropped_throttle = 0_u64;
    let mut ticker = interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                info!(
                    "[offchain_price] accepted={} dropped_stale={} dropped_conf={} dropped_throttle={}",
                    accepted,
                    dropped_stale,
                    dropped_conf,
                    dropped_throttle
                );
            }
            maybe_evt = price_rx.recv() => {
                let Some(evt) = maybe_evt else { break; };

                let age = now_secs().saturating_sub(evt.publish_time);
                if evt.publish_time > 0 && age > cfg.max_staleness_secs {
                    dropped_stale = dropped_stale.saturating_add(1);
                    continue;
                }

                if evt.confidence_bps > cfg.max_confidence_bps {
                    dropped_conf = dropped_conf.saturating_add(1);
                    continue;
                }

                let last = last_emit.get(&evt.asset).copied().unwrap_or(0);
                let now = now_ms();
                if now.saturating_sub(last) < cfg.min_rescan_interval_ms {
                    dropped_throttle = dropped_throttle.saturating_add(1);
                    continue;
                }
                last_emit.insert(evt.asset, now);

                if cfg.dispatch_price_updates {
                    let update = liquid_update(&evt);
                    if liq_tx.send(update).await.is_ok() {
                        accepted = accepted.saturating_add(1);
                    }
                }
            }
        }
    }

    error!("[offchain_price] event channel closed");
}

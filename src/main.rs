use anyhow::Result;
use alloy::primitives::Address;
use log::info;
use std::collections::{BTreeMap, HashMap, HashSet};
use tokio::task::JoinSet;

use rust::block_stream::stream_blocks_gossip;
use rust::config::Config;
use rust::filter_revm::{
    CombinedFilter,
    build_router_registry,
    fetch_top_tokens,
    filter_paths,
    filter_pools_full,
    gecko_to_address_set,
};
use rust::ignition::{start_pipeline, PipelineConfig};
use rust::path_builder::build_arb_paths;
use rust::pool_loader::{load_all_pools_from_cache, raw_to_pools_vec, raw_to_pool_metas};
use rust::utils::setup_logger;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    setup_logger()?;

    let config = Config::load()?;
    info!("Config loaded: chain={} id={}", config.chain.name, config.chain.chain_id);

    // Resolve runtime secrets from the environment
    let signer_key = std::env::var("SIGNING_KEY").unwrap_or_default();
    let bot_address: Address = std::env::var("BOT_ADDRESS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(Address::ZERO);
    let sequencer_url = config
        .execution
        .private_rpc_url
        .clone()
        .or_else(|| std::env::var("PRIVATE_RPC_URL").ok())
        // Chain-specific sequencer if set in [chain] (Base, OP, etc.).
        .or_else(|| config.chain.sequencer_url.clone())
        // Final fallback: submit directly to the local node RPC.
        .unwrap_or_else(|| config.chain.https_url.clone());
    let dry_run = std::env::var("DRY_RUN")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(true);

    // ── Compute native token + price pool map for profit normalisation ────────
    // `native_token`: the chain's gas token (e.g. WBNB on BSC). Profit from arbs
    // that start in any other token is price-converted to this unit before being
    // compared against gas cost. Falls back to Address::ZERO when gas config is absent.
    let native_token: Address = config
        .gas
        .token_address
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(Address::ZERO);

    // `token_price_pools`: for each non-native start token that has a `price_pool`
    // specified in the TOML, record the V2 pool used to look up the exchange rate.
    let token_price_pools: HashMap<Address, Address> = config
        .start_tokens
        .iter()
        .filter_map(|t| {
            let token_addr: Address = t.address.parse().ok()?;
            if token_addr == native_token {
                return None; // no conversion needed for the native token itself
            }
            let pool_addr: Address = t.price_pool.as_deref()?.parse().ok()?;
            Some((token_addr, pool_addr))
        })
        .collect();

    if !token_price_pools.is_empty() {
        info!(
            "Price pools registered for {} non-native start token(s): {:?}",
            token_price_pools.len(),
            token_price_pools.keys().collect::<Vec<_>>()
        );
    } else {
        info!("No price pools configured — profit comparison uses raw token units for all start tokens");
    }

    // Build the alloy+revm pipeline configuration
    let mut pipeline_config = PipelineConfig {
        rpc_url: config.chain.https_url.clone(),
        ws_url: config.chain.wss_url.clone(),
        sequencer_url,
        bot_address,
        signer_key,
        chain_id: config.chain.chain_id,
        channel_buffer: config.execution.channel_buffer,
        broadcast_capacity: config.execution.broadcast_capacity,
        dry_run,
        last_synced_block: std::env::var("CATCHUP_BLOCKS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0),
        use_flashloan: config.execution.use_flashloan,
        flash_loan_fee_bps: 0,
        flash_loan_provider: config.execution.default_flashloan_provider.clone(),
        native_token,
        token_price_pools,
        max_gas_limit: config.execution.max_gas_limit,
        profit_share_bps: config.execution.profit_share_bps,
        min_submit_profit_wei: config.execution.min_submit_profit_wei,
        min_sim_profit_wei: config.execution.min_sim_profit_wei,
        optimization_steps: config.execution.optimization_steps,
        min_input_eth: config.execution.min_input_eth,
        max_input_eth: config.execution.max_input_eth,
        max_paths_per_block: config.execution.max_paths_per_block,
        num_simulators: config.execution.num_simulators,
        searcher_gas_estimate: config.execution.searcher_gas_estimate,

        max_stale_blocks: config.execution.max_stale_blocks,
        strike_threshold: config.execution.strike_threshold,
        gas_params: config.gas_params.clone(),
        live_base_fee_wei: None,
        opportunities_log: String::new(), // set below after cache_dir is resolved
        enable_mempool: config.execution.enable_mempool,
        revm_filter_routers: config.revm_filter.routers.clone(),
        toxic_tokens_path: String::new(), // set below after cache_dir is resolved
    };

    // ── Startup seeder: load pools from CSV cache ─────────────────────────────
    // Reads `.cached-pools.csv` (V2) and `.cached-v3cl-pools.csv` (V3) from the
    // cache directory configured per chain.  The CSV files are populated by the
    // off-chain Python scripts in scripts/ (or by pool_loader's on-chain crawler
    // after the first run).
    let cache_dir = config
        .chain
        .cache_dir
        .as_deref()
        .unwrap_or_else(|| match config.chain.name.as_str() {
            "base" => "cache/base",
            "avax" => "cache/avax",
            "sonic" => "cache/sonic",
            name => {
                log::warn!("Unknown chain '{name}', defaulting cache_dir to cache/{name}");
                "cache/base" // safe fallback
            }
        })
        .to_string();

    let bot_dir = format!("bot/{}", config.chain.name.to_ascii_lowercase());
    if let Err(e) = std::fs::create_dir_all(&bot_dir) {
        log::warn!("Could not create bot log directory '{}': {}", bot_dir, e);
    }
    pipeline_config.opportunities_log = format!("{bot_dir}/opportunities.log");
    let toxic_cache_dir = if config.chain.name.eq_ignore_ascii_case("bsc") {
        "cache/bsc".to_string()
    } else {
        cache_dir.clone()
    };
    if let Err(e) = std::fs::create_dir_all(&toxic_cache_dir) {
        log::warn!("Could not create toxic token cache directory '{}': {}", toxic_cache_dir, e);
    }
    pipeline_config.toxic_tokens_path = format!("{toxic_cache_dir}/.toxic-tokens.toml");
    if !std::path::Path::new(&pipeline_config.toxic_tokens_path).exists() {
        let seed = "[toxic_tokens]\naddresses = []\n";
        if let Err(e) = std::fs::write(&pipeline_config.toxic_tokens_path, seed) {
            log::warn!(
                "Could not initialize toxic token cache '{}': {}",
                pipeline_config.toxic_tokens_path,
                e
            );
        }
    }

    // ── Dynamic flash-loan provider fee routing ─────────────────────────────
    // Maintains a persistent fee cache and selects the cheapest configured provider.
    // NOTE: on-chain `executeArbitrage` currently executes via AaveV3-only flash loans.
    // Selecting other providers affects simulation fee deduction and routing metadata,
    // but contract-side execution still requires Aave until the contract is upgraded.
    if config.execution.use_flashloan {
        let mut fee_map: BTreeMap<String, u64> = BTreeMap::new();
        fee_map.insert("AaveV3".to_string(), config.execution.flash_loan_fee_aave_bps);
        fee_map.insert(
            "UniswapV3".to_string(),
            config.execution.flash_loan_fee_uniswap_v3_bps,
        );
        fee_map.insert(
            "PancakeSwapV3".to_string(),
            config.execution.flash_loan_fee_pancakeswap_v3_bps,
        );

        let selected_providers: Vec<String> = if config.execution.flashloan_providers.is_empty() {
            vec![config.execution.default_flashloan_provider.clone()]
        } else {
            config.execution.flashloan_providers.clone()
        };

        let fee_cache_path = format!("{bot_dir}/flashloan-fees.toml");
        if let Some((provider, fee_bps)) = resolve_flashloan_provider(
            &selected_providers,
            &fee_map,
            &config.execution.default_flashloan_provider,
            config.execution.dynamic_flashloan_routing,
            &fee_cache_path,
        ) {
            pipeline_config.flash_loan_provider = provider.clone();
            pipeline_config.flash_loan_fee_bps = fee_bps;
            info!(
                "Flash-loan routing: provider={} fee_bps={} (dynamic routing from cache={})",
                provider,
                fee_bps,
                fee_cache_path
            );
        } else {
            pipeline_config.flash_loan_provider = "AaveV3".to_string();
            pipeline_config.flash_loan_fee_bps = config.execution.flash_loan_fee_aave_bps;
            log::warn!(
                "Flash-loan routing fallback: provider=AaveV3 fee_bps={}",
                pipeline_config.flash_loan_fee_bps
            );
        }
    } else {
        pipeline_config.flash_loan_provider = "none".to_string();
        pipeline_config.flash_loan_fee_bps = 0;
    }

    let mut raw_pools = load_all_pools_from_cache(&cache_dir)
        .unwrap_or_else(|e| {
            log::warn!("Could not load pool cache from '{cache_dir}': {e} — starting with empty pool set");
            vec![]
        });

    // Optional static blacklist filter from config.
    // Drops pools touching known scam/honeypot token addresses before path generation.
    let blacklisted: HashSet<Address> = config
        .chain
        .blacklisted_tokens
        .iter()
        .filter_map(|s| s.parse::<Address>().ok())
        .collect();
    if !blacklisted.is_empty() {
        let before = raw_pools.len();
        raw_pools.retain(|p| {
            !blacklisted.contains(&p.token0) && !blacklisted.contains(&p.token1)
        });
        info!(
            "Blacklist prefilter: {} -> {} pools ({} removed)",
            before,
            raw_pools.len(),
            before.saturating_sub(raw_pools.len())
        );
    }

    // ── Full startup pool filter ──────────────────────────────────────────────
    // Combines GeckoTerminal top-token filter + balance-slot detection (via RPC) +
    // REVM roundtrip swap test for every pool.
    //
    // Set SKIP_POOL_FILTER=1 to bypass the expensive REVM stage
    // (e.g. during development without a local node).
    // GeckoTerminal requires no API key.
    //
    // Filter results are cached to `{cache_dir}/.swap-filter-safe-pools.txt`.
    // On subsequent runs the cache is loaded and only new pools are re-tested.
    // Set FORCE_REFILTER=1 to discard the cache and re-run the full filter.
    if !raw_pools.is_empty() {
        let skip_revm = std::env::var("SKIP_POOL_FILTER")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);
        let force_refilter = std::env::var("FORCE_REFILTER")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);
        let gecko_limit = std::env::var("GECKO_TOP_TOKENS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(200);
        let gecko_enabled = std::env::var("ENABLE_GECKO_TERMINAL")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let filter_cache_path = format!("{cache_dir}/.swap-filter-safe-pools.txt");

        if !skip_revm {
            // ── Try loading cached filter results ──────────────────────────
            let cached_safe: Option<HashSet<Address>> = if !force_refilter {
                match std::fs::read_to_string(&filter_cache_path) {
                    Ok(contents) => {
                        let addrs: HashSet<Address> = contents
                            .lines()
                            .filter(|l| !l.is_empty() && !l.starts_with('#'))
                            .filter_map(|l| l.trim().parse::<Address>().ok())
                            .collect();
                        if addrs.is_empty() {
                            log::warn!("[FilterCache] Cache file exists but empty — will re-run filter");
                            None
                        } else {
                            log::info!("[FilterCache] Loaded {} safe pool addresses from cache", addrs.len());
                            Some(addrs)
                        }
                    }
                    Err(_) => {
                        log::info!("[FilterCache] No cache file found — will run full SwapFilter");
                        None
                    }
                }
            } else {
                log::info!("[FilterCache] FORCE_REFILTER=1 — ignoring cache");
                None
            };

            if let Some(safe_addrs) = cached_safe {
                // Fast path: use cached results — only keep pools that passed before
                let before = raw_pools.len();
                raw_pools.retain(|p| safe_addrs.contains(&p.address));
                log::info!(
                    "[FilterCache] Applied cache: {} → {} pools ({} removed)",
                    before, raw_pools.len(), before.saturating_sub(raw_pools.len())
                );
            } else {
                // Slow path: run the full REVM roundtrip filter
                let rpc = config.chain.https_url.clone();
                let router_registry = build_router_registry(&config.revm_filter.routers);
                info!(
                    "Loaded {} REVM router mappings from [revm_filter.routers]: {:?}",
                    router_registry.len(),
                    router_registry.keys().collect::<Vec<_>>()
                );
                raw_pools = filter_pools_full(
                    raw_pools,
                    &rpc,
                    &router_registry,
                    Some(""),   // api_key unused by DexScreener
                    if gecko_enabled { gecko_limit } else { 0 },
                    &blacklisted,
                )
                .await?;

                // ── Save filter results to cache ──────────────────────────
                let safe_addrs: Vec<String> = raw_pools
                    .iter()
                    .map(|p| format!("{:?}", p.address))
                    .collect();
                let cache_content = format!(
                    "# SwapFilter safe pools — generated {}\n# {} pools passed REVM roundtrip test\n{}\n",
                    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
                    safe_addrs.len(),
                    safe_addrs.join("\n")
                );
                match std::fs::write(&filter_cache_path, &cache_content) {
                    Ok(_) => log::info!(
                        "[FilterCache] Saved {} safe pool addresses to {}",
                        safe_addrs.len(), filter_cache_path
                    ),
                    Err(e) => log::warn!(
                        "[FilterCache] Failed to write cache file {}: {}",
                        filter_cache_path, e
                    ),
                }
            }
        }
        // SKIP_POOL_FILTER=1: lightweight path-level GeckoTerminal filter runs below
    }

    // Parse start tokens from config (addresses of tokens to arb FROM)
    let start_tokens: Vec<Address> = config
        .start_tokens
        .iter()
        .filter_map(|t| t.address.parse().ok())
        .collect();

    // Build arb paths from the (filtered) pool set.
    let mut paths = if raw_pools.is_empty() || start_tokens.is_empty() {
        log::warn!("No pools or start tokens — pipeline starts in listen-only mode");
        vec![]
    } else {
        build_arb_paths(&raw_pools, &start_tokens, config.execution.max_hops, config.execution.max_paths_per_token)
    };

    // Optional lightweight GeckoTerminal path filter when SKIP_POOL_FILTER=1.
    // Skipped entirely when the full pool filter already ran (which includes GeckoTerminal).
    let skip_revm = std::env::var("SKIP_POOL_FILTER")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);
    let gecko_enabled = std::env::var("ENABLE_GECKO_TERMINAL")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if skip_revm && gecko_enabled && !paths.is_empty() {
        let safe_pool_addrs: HashSet<Address> = raw_pools.iter().map(|p| p.address).collect();
        let mut combined = CombinedFilter::new(500);
        combined.add_safe_pools(safe_pool_addrs);

        let limit = std::env::var("GECKO_TOP_TOKENS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(200);

        match fetch_top_tokens("", &config.chain.name, limit).await {
            Ok(tokens) => {
                let allowed = gecko_to_address_set(&tokens);
                if !allowed.is_empty() {
                    combined.set_allowed_tokens(allowed);
                    let before = paths.len();
                    paths = filter_paths(paths, &combined);
                    info!(
                        "GeckoTerminal path-filter: {} -> {} paths ({} removed)",
                        before,
                        paths.len(),
                        before.saturating_sub(paths.len())
                    );
                }
            }
            Err(e) => {
                log::warn!("GeckoTerminal path-filter failed ({e}); continuing without token filter");
            }
        }
    } else if skip_revm && !gecko_enabled {
        info!("GeckoTerminal path-filter disabled (set ENABLE_GECKO_TERMINAL=1 to enable)");
    }

    let pool_metas = raw_to_pool_metas(&raw_pools);
    let pools = raw_to_pools_vec(&raw_pools);

    info!(
        "Startup seeder: {} pools, {} start tokens, {} arb paths",
        raw_pools.len(),
        start_tokens.len(),
        paths.len()
    );
    // ── Live eth_gasPrice seed ───────────────────────────────────────────────────────
    // On chains like BSC, block headers always report baseFeePerGas=0x0, so the
    // GasStation would otherwise use only the config floor. Fetching eth_gasPrice
    // once at startup gives us the real network gas price as an accurate seed.
    {
        use alloy::providers::{Provider, ProviderBuilder};
        let rpc_url: alloy::transports::http::reqwest::Url =
            pipeline_config.rpc_url.parse().expect("invalid rpc_url");
        let provider = ProviderBuilder::new().on_http(rpc_url);
        match provider.get_gas_price().await {
            Ok(gas_price_u128) => {
                let live_wei = gas_price_u128.min(u64::MAX as u128) as u64;
                info!(
                    "[Startup] eth_gasPrice = {} wei ({:.4} gwei) — will seed GasStation base_fee",
                    live_wei,
                    live_wei as f64 / 1e9
                );
                pipeline_config.live_base_fee_wei = Some(live_wei);
            }
            Err(e) => {
                log::warn!(
                    "[Startup] eth_gasPrice fetch failed ({e}); GasStation will use config min_base_fee_wei={}",
                    pipeline_config.gas_params.min_base_fee_wei
                );
            }
        }
    }
    // ── Start the alloy + revm pipeline ──────────────────────────────────────
    let pipeline  = start_pipeline(pipeline_config, paths, pool_metas, pools);
    info!("Pipeline started (dry_run={dry_run}, chain_id={})", config.chain.chain_id);

    // ── Stream new blocks from all WS endpoints (gossip dedup) ─────────────
    let mut set = JoinSet::new();
    let mut all_ws_urls = vec![config.chain.wss_url.clone()];
    all_ws_urls.extend(config.chain.wss_urls.iter().cloned());
    // Remove empty strings (unconfigured entries)
    all_ws_urls.retain(|u| !u.is_empty());
    info!("Gossip: {} WS endpoint(s) configured", all_ws_urls.len());
    let gossip_tx = pipeline.block_tx.clone();
    set.spawn(async move {
        stream_blocks_gossip(all_ws_urls, gossip_tx).await;
    });

    info!("Arbitrage engine running — listening for blocks");

    while let Some(res) = set.join_next().await {
        if let Err(e) = res {
            log::error!("Pipeline task panicked: {e:?}");
        }
    }

    Ok(())
}

fn canonical_provider_name(name: &str) -> String {
    match name.trim().to_ascii_lowercase().as_str() {
        "aave" | "aavev3" => "AaveV3".to_string(),
        "uniswap" | "uniswapv3" | "univ3" => "UniswapV3".to_string(),
        "pancake" | "pancakeswap" | "pancakeswapv3" | "pcsv3" => "PancakeSwapV3".to_string(),
        other => other.to_string(),
    }
}

fn resolve_flashloan_provider(
    selected_providers: &[String],
    provider_fees_bps: &BTreeMap<String, u64>,
    default_provider: &str,
    dynamic: bool,
    cache_path: &str,
) -> Option<(String, u64)> {
    let mut candidates: Vec<(String, u64)> = selected_providers
        .iter()
        .map(|p| canonical_provider_name(p))
        .filter_map(|p| provider_fees_bps.get(&p).copied().map(|fee| (p, fee)))
        .collect();

    if candidates.is_empty() {
        let fallback = canonical_provider_name(default_provider);
        let fee = provider_fees_bps.get(&fallback).copied()?;
        candidates.push((fallback, fee));
    }

    let selected = if dynamic {
        candidates.into_iter().min_by_key(|(_, fee)| *fee)?
    } else {
        let fallback = canonical_provider_name(default_provider);
        provider_fees_bps
            .get(&fallback)
            .copied()
            .map(|fee| (fallback, fee))
            .unwrap_or_else(|| candidates[0].clone())
    };

    let parent = std::path::Path::new(cache_path).parent();
    if let Some(dir) = parent {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut providers = toml::map::Map::new();
    for (name, fee) in provider_fees_bps {
        providers.insert(name.clone(), toml::Value::Integer(*fee as i64));
    }
    let mut root = toml::map::Map::new();
    root.insert("selected_provider".to_string(), toml::Value::String(selected.0.clone()));
    root.insert("selected_fee_bps".to_string(), toml::Value::Integer(selected.1 as i64));
    root.insert(
        "updated_utc".to_string(),
        toml::Value::String(chrono::Utc::now().to_rfc3339()),
    );
    root.insert("providers".to_string(), toml::Value::Table(providers));
    let _ = std::fs::write(cache_path, toml::Value::Table(root).to_string());

    Some(selected)
}

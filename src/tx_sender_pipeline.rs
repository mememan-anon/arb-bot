/// Transaction sender pipeline worker — receives ValidPath events,
/// builds and signs EIP-1559 transactions, and submits directly to
/// the Base L2 sequencer.
///
/// Pipeline position: ValidPath → [TxSender] → chain
///
/// On Base (Optimism stack), transactions go directly to the sequencer
/// rather than a public mempool. This means:
/// - No frontrunning risk from other searchers
/// - Need to submit as fast as possible after finding an arb
/// - Priority fee matters for ordering within the block

use alloy::primitives::{Address, U256};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::gas_station::GasStation;
use crate::pipeline_events::ValidPath;

/// Configuration for the transaction sender.
pub struct TxSenderConfig {
    /// RPC URL for the sequencer (direct submission endpoint).
    pub sequencer_url: String,
    /// RPC URL for nonce seeding and gas queries (local node, not sequencer).
    /// The Base sequencer only accepts eth_sendRawTransaction — all other
    pub rpc_url: String,
    /// Bot contract address (receives the flash loan and executes swaps).
    pub bot_address: Address,
    /// Signer private key (hex, without 0x prefix).
    /// In production, use a proper key management solution.
    pub signer_key: String,
    /// Chain ID (Base = 8453, Base Sepolia = 84532).
    pub chain_id: u64,
    /// Maximum gas limit for bot transactions.
    pub max_gas_limit: u64,
    /// Share of profit to spend on priority fee (bps).
    pub profit_share_bps: u64,
    /// Minimum net profit to actually submit (safety threshold).
    pub min_submit_profit_wei: U256,
    /// Maximum stale lag tolerated before dropping an arb.
    pub max_stale_blocks: u64,
    /// Unified path/token strike threshold.
    pub strike_threshold: u32,
    /// Whether to actually submit transactions (false = dry run).
    pub dry_run: bool,
    /// Path to the opportunities log file (e.g. cache/bsc/opportunities.log).
    /// Every ValidPath that passes the profit threshold is appended here.
    pub opportunities_log: String,
    /// Flash-loan provider selector encoded in contract calldata.
    /// 0=AaveV3, 1=UniswapV3, 2=PancakeSwapV3, 255=direct/no-flash.
    pub flash_loan_provider: u8,
}

impl Default for TxSenderConfig {
    fn default() -> Self {
        Self {
            sequencer_url: String::new(),
            rpc_url: String::new(),
            bot_address: Address::ZERO,
            signer_key: String::new(),
            chain_id: 0,
            max_gas_limit: 1_000_000,
            profit_share_bps: 5000,
            min_submit_profit_wei: U256::from(5_000_000_000_000u64), // 0.000005 BNB (~$0.003)
            max_stale_blocks: 3,
            strike_threshold: 3,
            dry_run: true, // safe default
            opportunities_log: String::new(),
            flash_loan_provider: 255,
        }
    }
}

/// Transaction sender worker.
pub struct TxSenderWorker {
    pub config: TxSenderConfig,
    pub gas_station: Arc<GasStation>,
    client: reqwest::Client,
    /// Monotonically increasing nonce for signed transactions.
    /// Seeded from `eth_getTransactionCount` at startup in `run()`.
    nonce: AtomicU64,
    /// Shared path blacklist — paths that revert on preflight are added here
    /// so simulator workers skip them in future blocks.
    pub blacklisted_paths: Arc<Mutex<HashSet<u64>>>,
    /// Shared token blacklist — tokens that repeatedly cause preflight reverts
    /// (honeypots, tax tokens) are blacklisted so ALL paths containing them are skipped.
    pub blacklisted_tokens: Arc<Mutex<HashSet<Address>>>,
    /// Start tokens (WBNB, USDT, etc.) that should NEVER be blacklisted.
    pub start_tokens: HashSet<Address>,
    /// Path to persistent toxic-token blacklist TOML file.
    /// New toxic tokens are persisted here so they survive restarts.
    pub toxic_tokens_path: String,
}

impl TxSenderWorker {
    pub fn new(
        config: TxSenderConfig,
        gas_station: Arc<GasStation>,
        blacklisted_paths: Arc<Mutex<HashSet<u64>>>,
        blacklisted_tokens: Arc<Mutex<HashSet<Address>>>,
        start_tokens: HashSet<Address>,
        toxic_tokens_path: String,
    ) -> Self {
        // Build a warm, pooled HTTP client matching BaseBuster's TransactionSender::new()
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(None)
            .tcp_keepalive(Duration::from_secs(10))
            .tcp_nodelay(true)
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            config,
            gas_station,
            client,
            nonce: AtomicU64::new(0), // seeded from chain in run()
            blacklisted_paths,
            blacklisted_tokens,
            start_tokens,
            toxic_tokens_path,
        }
    }

    /// Fetch the current nonce from the chain and store it.
    /// Called once at the start of `run()` before processing any transactions.
    async fn seed_nonce_from_rpc(&self) {
        use alloy::providers::{Provider, ProviderBuilder};
        use alloy::signers::k256::SecretKey;
        use alloy::signers::local::PrivateKeySigner;

        if self.config.signer_key.is_empty() {
            log::warn!("[TxSender] signer_key not set — starting nonce at 0");
            return;
        }
        // Derive the signer EOA address from the private key.
        // We must query the EOA nonce, NOT the bot contract address
        // (contracts have an internal nonce that is unrelated to tx signing).
        let signer_address = match alloy::hex::decode(&self.config.signer_key)
            .ok()
            .and_then(|bytes| SecretKey::from_bytes((&bytes[..]).into()).ok())
            .map(|sk| PrivateKeySigner::from(sk))
        {
            Some(signer) => signer.address(),
            None => {
                log::warn!("[TxSender] Failed to derive signer address — starting nonce at 0");
                return;
            }
        };
        // Use the local node (rpc_url), NOT the sequencer — the Base sequencer
        // only accepts eth_sendRawTransaction and returns 403 for all other methods.
        let url = match self.config.rpc_url.parse() {
            Ok(u) => u,
            Err(e) => {
                log::warn!("[TxSender] Bad rpc_url: {e} — starting nonce at 0");
                return;
            }
        };
        let provider = ProviderBuilder::new().on_http(url);
        match provider.get_transaction_count(signer_address).await {
            Ok(count) => {
                self.nonce.store(count, Ordering::SeqCst);
                log::info!("[TxSender] Nonce seeded from chain: {count} (signer: {signer_address})");
            }
            Err(e) => {
                log::warn!("[TxSender] eth_getTransactionCount failed: {e} — starting at 0");
            }
        }
    }

    /// Warm up the HTTP connection to the sequencer (pre-opens TCP + TLS).
    /// Called once at startup so the first real submission has zero cold-start.
    async fn warmup_sequencer_connection(&self) {
        let warmup_json = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_blockNumber",
            "params": [],
            "id": 1
        });
        match self
            .client
            .post(&self.config.sequencer_url)
            .json(&warmup_json)
            .send()
            .await
        {
            Ok(_) => log::info!("[TxSender] Sequencer connection warmed up"),
            Err(e) => log::warn!("[TxSender] Warmup ping failed (non-fatal): {e}"),
        }
    }

    /// Query the current block number from the RPC node.
    async fn get_chain_head(&self) -> Option<u64> {
        use alloy::providers::{Provider, ProviderBuilder};
        let url = self.config.rpc_url.parse().ok()?;
        let provider = ProviderBuilder::new().on_http(url);
        provider.get_block_number().await.ok()
    }

    /// Run the transaction sender worker.
    pub async fn run(&self, mut valid_rx: mpsc::Receiver<ValidPath>) {
        log::info!(
            "[TxSender] Worker started (dry_run={})",
            self.config.dry_run
        );

        // Seed nonce from chain and warm up the connection on startup
        self.seed_nonce_from_rpc().await;
        self.warmup_sequencer_connection().await;

        // Track chain head to skip stale arbs during catch-up.
        let mut last_head_check = std::time::Instant::now();
        let mut cached_head: u64 = self.get_chain_head().await.unwrap_or(0);
        let mut strike_counts: HashMap<u64, u32> = HashMap::new();
        let mut token_strike_counts: HashMap<Address, u32> = HashMap::new();
        log::info!("[TxSender] Chain head at startup: {cached_head}");

        while let Some(valid) = valid_rx.recv().await {
            // Safety check
            if valid.net_profit < self.config.min_submit_profit_wei {
                log::debug!(
                    "[TxSender] Skipping path: profit {} below threshold",
                    valid.net_profit
                );
                continue;
            }

            // Refresh chain head every 1 second (avoid spamming RPC)
            if last_head_check.elapsed() > Duration::from_secs(1) {
                if let Some(head) = self.get_chain_head().await {
                    cached_head = head;
                }
                last_head_check = std::time::Instant::now();
            }

            // Skip stale arbs — if the simulated block is behind chain head,
            // the on-chain state has already moved and the arb will revert.
            if cached_head > 0 && valid.arb.block_number + self.config.max_stale_blocks < cached_head {
                log::debug!(
                    "[TxSender] Skipping stale arb from block {} (chain head: {cached_head})",
                    valid.arb.block_number
                );
                continue;
            }

            // Log every opportunity that passes the threshold to disk.
            if !self.config.opportunities_log.is_empty() {
                let mode = if self.config.dry_run { "DRY_RUN" } else { "LIVE" };
                let hops = valid.arb.path.steps.len();
                let pools: Vec<String> = valid.arb.path.steps
                    .iter()
                    .map(|s| format!("{:?}", s.pool_address))
                    .collect();
                let line = format!(
                    "block={} profit_wei={} input_wei={} hops={} pools=[{}] mode={}\n",
                    valid.arb.block_number,
                    valid.net_profit,
                    valid.amount_in,
                    hops,
                    pools.join(","),
                    mode,
                );
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.config.opportunities_log)
                {
                    let _ = f.write_all(line.as_bytes());
                }
            }

            match self.build_and_send(&valid).await {
                Ok(tx_hash) => {
                    log::info!(
                        "[TxSender] Block {}: Submitted tx {} | profit: {} wei | input: {}",
                        valid.arb.block_number,
                        tx_hash,
                        valid.net_profit,
                        valid.amount_in,
                    );
                }
                Err(e) => {
                    // 3-strike auto-blacklist: paths that repeatedly fail preflight
                    // have toxic tokens (tax, honeypot, transfer-blocked).
                    // Only blacklist after configured consecutive failures.
                    //
                    // IMPORTANT: "repay insufficient" means the arb was marginal
                    // (swap succeeded but output < flash loan repay). This is a
                    // pricing/staleness issue, NOT a toxic token. Don't count strikes.
                    if e.contains("preflight eth_call reverted") {
                        let ph = valid.arb.path.hash;

                        // Log full path details for debugging reverts
                        let path_desc: Vec<String> = valid.arb.path.steps.iter().map(|s| {
                            format!("{:?}→{:?} via {:?}({:?})", s.token_in, s.token_out, s.pool_address, s.protocol)
                        }).collect();
                        log::debug!(
                            "[TxSender] REVERT path {:x} steps=[{}] amount_in={}",
                            ph, path_desc.join(" | "), valid.amount_in,
                        );

                        // "repay insufficient" = marginal arb, don't blacklist
                        let is_repay_insufficient = e.contains("repay insufficient")
                            || e.contains("726570617920696e73756666696369656e74"); // hex-encoded
                        // INSTANT_BLACKLIST = TF/transfer-failed, skip 3-strike delay
                        let is_instant_bl = e.contains("INSTANT_BLACKLIST");
                        if is_repay_insufficient {
                            log::info!(
                                "[TxSender] Block {}: Marginal arb (repay insufficient) for path {:x} — NOT counting strike",
                                valid.arb.block_number, ph,
                            );
                        } else {
                            let count = strike_counts.entry(ph).or_insert(0);
                            if is_instant_bl { *count = self.config.strike_threshold; }
                            else { *count += 1; }

                            if *count >= self.config.strike_threshold {
                                if let Ok(mut bl) = self.blacklisted_paths.lock() {
                                    bl.insert(ph);
                                    log::warn!(
                                        "[TxSender] Block {}: Preflight revert strike {}/{} → BLACKLISTED path {:x} ({} total) | {e}",
                                        valid.arb.block_number,
                                        count,
                                        self.config.strike_threshold,
                                        ph,
                                        bl.len(),
                                    );
                                }
                            } else {
                                log::info!(
                                    "[TxSender] Block {}: Preflight revert strike {}/{} for path {:x} | {e}",
                                    valid.arb.block_number,
                                    count,
                                    self.config.strike_threshold,
                                    ph,
                                );
                            }
                            // Token-level blacklisting: instant-blacklist skips strike accumulation.
                            for step in &valid.arb.path.steps {
                                for tok in [step.token_in, step.token_out] {
                                    if self.start_tokens.contains(&tok) { continue; }
                                    let tc = token_strike_counts.entry(tok).or_insert(0);
                                    if is_instant_bl { *tc = self.config.strike_threshold; }
                                    else { *tc += 1; }
                                    if *tc >= self.config.strike_threshold {
                                        if let Ok(mut bt) = self.blacklisted_tokens.lock() {
                                            if bt.insert(tok) {
                                                let _ = self.persist_toxic_token_file(tok);
                                                if bt.len() <= 50 || bt.len() % 100 == 0 {
                                                    log::warn!(
                                                        "[TxSender] Block {}: Token {:?} blacklisted ({} total toxic tokens)",
                                                        valid.arb.block_number, tok, bt.len()
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        log::warn!(
                            "[TxSender] Block {}: Failed to send: {e}",
                            valid.arb.block_number,
                        );
                    }
                }
            }
        }

        log::info!("[TxSender] Channel closed, shutting down");
    }

    fn persist_toxic_token_file(&self, token: Address) -> Result<(), String> {
        if self.toxic_tokens_path.is_empty() {
            return Ok(());
        }

        let token_str = format!("{:?}", token);
        let path = std::path::Path::new(&self.toxic_tokens_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create toxic token dir failed: {e}"))?;
        }

        let mut addresses: Vec<String> = Vec::new();
        if let Ok(contents) = std::fs::read_to_string(path) {
            // Canonical format: TOML
            // [toxic_tokens]
            // addresses = ["0x...", "0x..."]
            if let Ok(value) = toml::from_str::<toml::Value>(&contents) {
                if let Some(items) = value
                    .get("toxic_tokens")
                    .and_then(|t| t.get("addresses"))
                    .and_then(|a| a.as_array())
                {
                    for item in items {
                        if let Some(addr) = item.as_str() {
                            addresses.push(addr.to_string());
                        }
                    }
                }
            }

            // Backward-compatible fallback for legacy line-based files.
            if addresses.is_empty() {
                for line in contents.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    addresses.push(line.to_string());
                }
            }
        }

        if !addresses.iter().any(|a| a.eq_ignore_ascii_case(&token_str)) {
            addresses.push(token_str);
            addresses.sort();
            addresses.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        }

        let body = format!(
            "[toxic_tokens]\naddresses = {}\n",
            toml::Value::Array(
                addresses
                    .iter()
                    .map(|a| toml::Value::String(a.clone()))
                    .collect()
            )
            .to_string()
        );

        std::fs::write(path, body)
            .map_err(|e| format!("persist toxic token file failed: {e}"))
    }

    /// Build and send the transaction, trying flash-loan providers in order.
    ///
    /// Provider priority: primary first (UniswapV3/PancakeSwapV3 = 0 fee), then
    /// AaveV3 as fallback for tokens those flash pools don't carry (ETH, BTCB…).
    /// Transfer-failed reverts propagate immediately with an INSTANT_BLACKLIST tag.
    async fn build_and_send(&self, valid: &ValidPath) -> Result<String, String> {
        // Calculate gas parameters (shared across all provider attempts).
        let base_fee = self.gas_station.predict_next_base_fee();
        let profit_u64 = valid.net_profit.min(U256::from(u64::MAX)).to::<u64>();
        let mut priority_fee = self.gas_station.calc_priority_fee(
            profit_u64,
            valid.arb.gas_estimate,
            self.config.profit_share_bps,
        );
        let min_effective = 5_000_000u64; // 5 mwei
        if base_fee.saturating_add(priority_fee) < min_effective {
            priority_fee = min_effective.saturating_sub(base_fee);
        }
        let max_fee_per_gas = base_fee.saturating_add(priority_fee).saturating_mul(2);

        // Provider order: primary (e.g. UniswapV3) then AaveV3 as fallback.
        let primary = self.config.flash_loan_provider;
        let providers: Vec<u8> = if primary != 0 { vec![primary, 0] } else { vec![0] };

        let skip_preflight = std::env::var("SKIP_PREFLIGHT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let mut last_err = String::new();
        for (idx, &provider) in providers.iter().enumerate() {
            let calldata = self.encode_calldata_with_provider(valid, provider)?;

            if self.config.dry_run {
                log::info!(
                    "[TxSender] DRY RUN | provider={} block: {} | profit: {} wei | base_fee: {} | priority: {} | calldata: {} bytes",
                    provider, valid.arb.block_number, valid.net_profit, base_fee, priority_fee, calldata.len(),
                );
                return Ok("0x_dry_run".to_string());
            }

            if !skip_preflight {
                match self.preflight_eth_call(&calldata).await {
                    Ok(()) => {} // preflight passed — proceed to submit
                    Err(e) => {
                        // Transfer-failed / honeypot: signal instant blacklist.
                        if Self::is_instant_blacklist_error(&e) {
                            return Err(format!(
                                "preflight eth_call reverted: INSTANT_BLACKLIST {}",
                                e
                            ));
                        }
                        // Provider may not support this token — try next.
                        if idx + 1 < providers.len() {
                            log::info!(
                                "[TxSender] Provider {} preflight failed, trying fallback {}: {}",
                                provider, providers[idx + 1], &e[..e.len().min(120)]
                            );
                            last_err = e;
                            continue;
                        }
                        return Err(format!("preflight eth_call reverted: {e}"));
                    }
                }
            } else {
                log::info!(
                    "[TxSender] SKIP_PREFLIGHT: provider={} profit={} gas_est={}",
                    provider, valid.net_profit, valid.arb.gas_estimate,
                );
            }

            let tx = self.build_raw_tx(calldata, max_fee_per_gas, priority_fee).await?;
            return self.submit_raw_tx(&tx).await;
        }

        Err(format!("preflight eth_call reverted: {last_err}"))
    }

    /// Simulate the arb tx via `eth_call` against the node's latest state.
    /// Returns `Ok(())` if the call succeeds, or `Err(reason)` if it reverts.
    async fn preflight_eth_call(&self, calldata: &[u8]) -> Result<(), String> {
        let call_data_hex = format!("0x{}", hex::encode(calldata));
        // Derive the signer address so we send the eth_call from the correct EOA
        // (the contract checks `msg.sender == owner`).
        let from_addr = {
            use alloy::signers::k256::SecretKey;
            use alloy::signers::local::PrivateKeySigner;
            let key_bytes = alloy::hex::decode(&self.config.signer_key)
                .map_err(|e| format!("preflight: bad key hex: {e}"))?;
            let sk = SecretKey::from_bytes((&key_bytes[..]).into())
                .map_err(|e| format!("preflight: bad key: {e}"))?;
            let signer = PrivateKeySigner::from(sk);
            format!("{:?}", signer.address())
        };
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_call",
            "params": [{
                "from": from_addr,
                "to": format!("{:?}", self.config.bot_address),
                "data": call_data_hex,
                "gas": format!("0x{:x}", self.config.max_gas_limit),
            }, "pending"],
            "id": 1
        });

        let resp = self
            .client
            .post(&self.config.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("preflight eth_call request failed: {e}"))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("preflight eth_call JSON parse failed: {e}"))?;

        if let Some(err) = json.get("error") {
            return Err(format!("preflight eth_call reverted: {err}"));
        }
        // Some nodes return a result with 0x for successful void calls,
        // or a revert payload in the result field itself — check both.
        if let Some(result) = json.get("result").and_then(|r| r.as_str()) {
            // A "0x" result from a void function is success.
            // If the result starts with the Solidity Error(string) selector 0x08c379a2
            // it's a revert reason encoded in the result (some nodes do this).
            if result.starts_with("0x08c379a2") {
                return Err(format!("preflight eth_call reverted (Error selector in result): {result}"));
            }
            Ok(())
        } else {
            // No result field at all — treat as failure
            Err(format!("preflight eth_call: unexpected response: {json}"))
        }
    }

    /// Encode the calldata for the bot contract.
    ///
    /// Uses `FlashSwap::executeArbitrageCall` — the on-chain execution ABI,
    /// NOT the quoter ABI (FlashQuoter is read-only simulation only).
    fn encode_calldata_with_provider(&self, valid: &ValidPath, provider: u8) -> Result<Vec<u8>, String> {
        use alloy::sol_types::SolCall;
        use crate::gen_alloy::FlashSwap;

        let params = valid.arb.path.to_quoter_params(valid.amount_in);
        // Convert FlashQuoter::SwapParams → FlashSwap::SwapParams (same layout, same fees)
        let swap_params = FlashSwap::SwapParams {
            pools: params.pools,
            poolVersions: params.poolVersions,
            fees: params.fees,
            amountIn: params.amountIn,
            startToken: params.startToken,
            flashLoanProvider: provider,
        };
        let call = FlashSwap::executeArbitrageCall { arb: swap_params };
        Ok(call.abi_encode())
    }

    /// True if the revert signals a honeypot / transfer-blocked token that should
    /// be instantly blacklisted — skipping the normal 3-strike accumulation delay.
    ///
    /// Patterns:
    /// - `"data":"0x"` — empty revert, typical of safeTransfer("TF") failure
    /// - hex `5446` in data — ASCII "TF" (Transfer Failed) from UniswapV2 TransferHelper
    fn is_instant_blacklist_error(e: &str) -> bool {
        // Empty revert data: transfer was blocked (honeypot / tax on transfer)
        let has_empty_data = e.contains("\"data\":\"0x\"");
        // "TF" hex in payload (must not be a repay-related revert)
        let has_tf = e.contains("5446") && !e.contains("repay");
        has_empty_data || has_tf
    }

    /// Build a raw signed EIP-1559 transaction using the configured private key.
    async fn build_raw_tx(
        &self,
        calldata: Vec<u8>,
        max_fee_per_gas: u64,
        max_priority_fee: u64,
    ) -> Result<Vec<u8>, String> {
        use alloy::{
            eips::eip2718::Encodable2718,
            hex,
            network::{EthereumWallet, TransactionBuilder},
            primitives::Bytes as AlloyBytes,
            rpc::types::TransactionRequest,
            signers::{k256::SecretKey, local::PrivateKeySigner},
        };

        if self.config.signer_key.is_empty() {
            return Err("No signer key configured — set signer_key in PipelineConfig".to_string());
        }

        // Decode and import the private key
        let key_bytes = hex::decode(&self.config.signer_key)
            .map_err(|e| format!("Invalid signer_key hex: {e}"))?;
        let secret_key = SecretKey::from_bytes((&key_bytes[..]).into())
            .map_err(|e| format!("Invalid secret key bytes: {e}"))?;
        let signer = PrivateKeySigner::from(secret_key);
        let wallet = EthereumWallet::from(signer);

        // Fetch-and-increment nonce  (seed from eth_getTransactionCount on startup in production)
        let nonce = self.nonce.fetch_add(1, Ordering::SeqCst);

        // Build EIP-1559 transaction
        let tx = TransactionRequest::default()
            .with_to(self.config.bot_address)
            .with_nonce(nonce)
            .with_gas_limit(self.config.max_gas_limit)
            .with_chain_id(self.config.chain_id)
            .with_max_fee_per_gas(max_fee_per_gas as u128)
            .with_max_priority_fee_per_gas(max_priority_fee as u128)
            .transaction_type(2)
            .with_input(AlloyBytes::from(calldata));

        // Sign and RLP-encode as an EIP-2718 envelope
        let tx_envelope = tx
            .build(&wallet)
            .await
            .map_err(|e| format!("Transaction signing failed: {e}"))?;

        let mut encoded = Vec::new();
        tx_envelope.encode_2718(&mut encoded);
        Ok(encoded)
    }

    /// Submit a raw transaction to the sequencer via eth_sendRawTransaction.
    async fn submit_raw_tx(&self, raw_tx: &[u8]) -> Result<String, String> {
        let tx_hex = format!("0x{}", hex::encode(raw_tx));
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_sendRawTransaction",
            "params": [tx_hex],
            "id": 1
        });

        let resp = self
            .client
            .post(&self.config.sequencer_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Sequencer request failed: {e}"))?;

        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("JSON parse failed: {e}"))?;

        if let Some(result) = json.get("result").and_then(|r| r.as_str()) {
            Ok(result.to_string())
        } else {
            let err = json.get("error").map(|e| e.to_string()).unwrap_or_default();
            Err(format!("eth_sendRawTransaction failed: {err}"))
        }
    }
}

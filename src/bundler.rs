use anyhow::{anyhow, Result};
use ethers::prelude::*;
use ethers::types::{
    transaction::{eip2718::TypedTransaction, eip2930::AccessList},
    Address, Eip1559TransactionRequest, U256,
};
use ethers::{
    abi,
    middleware::MiddlewareBuilder,
    providers::{Http, Middleware, Provider},
    signers::{LocalWallet, Signer},
};
use ethers_flashbots::*;
use std::{str::FromStr, sync::Arc};
use url::Url;

use crate::config::Config;
use crate::constants::Env;

abigen!(
    ArbBot,
    r#"[
        function recoverToken(address token)
        function approveRouter(address router, address[] tokens, bool force)
        function algebraSwapExactInputSingle(address router, address tokenIn, address tokenOut, uint256 amountIn, uint256 amountOutMinimum) returns (uint256)
    ]"#,
);

#[derive(Debug, Clone)]
pub struct PathParam {
    pub router: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub pool_type: u8,
    /// Pool fee in bps (e.g. 71 for Blackhole, 300 for standard Uniswap V2).
    /// Encoded as the 5th slot per hop in calldata so the contract can use
    /// the correct fee for amount-out calculation and flashswap repayment.
    pub fee: u64,
}

impl PathParam {
    pub fn make_params(&self) -> Vec<abi::Token> {
        vec![
            abi::Token::Address(self.router.into()),
            abi::Token::Address(self.token_in.into()),
            abi::Token::Address(self.token_out.into()),
            abi::Token::Uint(U256::from(self.pool_type)),
            abi::Token::Uint(U256::from(self.fee)),
        ]
    }
}

#[derive(Debug, Clone)]
pub enum Flashloan {
    NotUsed = 0,
    Balancer = 1,
    UniswapV2 = 2,
    AaveV3 = 3,
}

type SignerProvider = SignerMiddleware<Provider<Http>, LocalWallet>;

pub struct Bundler {
    pub config: Config,
    pub env: Env,
    pub sender: LocalWallet,
    pub bot: ArbBot<SignerProvider>,
    pub provider: SignerProvider,
    pub flashbots: SignerMiddleware<FlashbotsMiddleware<SignerProvider, LocalWallet>, LocalWallet>,
    /// When set, all arb txs are broadcast here instead of the public RPC.
    /// Tx confirmation is still polled via the main `provider`.
    pub private_rpc_url: Option<String>,
    /// Reused HTTP client — avoids allocating a TLS context + connection pool on every tx.
    pub http_client: reqwest::Client,
}

impl Bundler {
    pub fn new() -> anyhow::Result<Self> {
        let config = Config::load()?;
        let env = Env::from_config(&config);

        let sender = env
            .private_key
            .as_deref()
            .ok_or_else(|| anyhow!("PRIVATE_KEY not set — required for execution mode"))?
            .parse::<LocalWallet>()
            .map_err(|e| anyhow!("invalid PRIVATE_KEY: {e}"))?
            .with_chain_id(env.chain_id.as_u64());
        let signer = env
            .signing_key
            .as_deref()
            .ok_or_else(|| anyhow!("SIGNING_KEY not set — required for execution mode"))?
            .parse::<LocalWallet>()
            .map_err(|e| anyhow!("invalid SIGNING_KEY: {e}"))?
            .with_chain_id(env.chain_id.as_u64());

        let provider = Provider::<Http>::try_from(&env.https_url)
            .map_err(|e| anyhow!("invalid RPC URL '{}': {e}", env.https_url))?
            .with_signer(sender.clone());

        let flashbots = SignerMiddleware::new(
            FlashbotsMiddleware::new(
                provider.clone(),
                Url::parse("https://relay.flashbots.net").unwrap(),
                signer,
            ),
            sender.clone(),
        );

        let bot_addr = env
            .bot_address
            .as_deref()
            .ok_or_else(|| anyhow!("BOT_ADDRESS not set — start in scan-only mode or set it in .env"))?
            .parse::<Address>()
            .map_err(|e| anyhow!("invalid BOT_ADDRESS: {e}"))?;
        let client = Arc::new(provider.clone());
        let bot = ArbBot::new(bot_addr, client.clone());

        Ok(Self {
            private_rpc_url: env.private_rpc_url.clone(),
            http_client: reqwest::Client::new(),
            config,
            env,
            sender,
            bot,
            provider,
            flashbots,
        })
    }

    pub fn aave_v3_pool(&self) -> Result<Address> {
        let cfg = self
            .config
            .aave_v3
            .as_ref()
            .ok_or_else(|| anyhow!("aave_v3 config missing"))?;
        Ok(Address::from_str(&cfg.pool).map_err(|e| anyhow!("{e}"))?)
    }

    pub async fn _common_fields(&self) -> Result<(H160, U256, U64)> {
        let nonce = self
            .provider
            .get_transaction_count(self.sender.address(), None)
            .await?;
        Ok((self.sender.address(), U256::from(nonce), self.env.chain_id))
    }

    pub async fn sign_tx(&self, tx: Eip1559TransactionRequest) -> Result<Bytes> {
        let typed = TypedTransaction::Eip1559(tx);
        let signature = self.sender.sign_transaction(&typed).await?;
        let signed = typed.rlp_signed(&signature);
        Ok(signed)
    }

    pub fn to_bundle<T: Into<BundleTransaction>>(
        &self,
        signed_txs: Vec<T>,
        block_number: U64,
    ) -> BundleRequest {
        let mut bundle = BundleRequest::new();

        for tx in signed_txs {
            let bundle_tx: BundleTransaction = tx.into();
            bundle = bundle.push_transaction(bundle_tx);
        }

        bundle
            .set_block(block_number + 1)
            .set_simulation_block(block_number)
            .set_simulation_timestamp(0)
    }

    pub async fn send_bundle(&self, bundle: BundleRequest) -> Result<TxHash> {
        let simulated = self.flashbots.inner().simulate_bundle(&bundle).await?;

        for tx in &simulated.transactions {
            if let Some(e) = &tx.error {
                return Err(anyhow!("Simulation error: {:?}", e));
            }
            if let Some(r) = &tx.revert {
                return Err(anyhow!("Simulation revert: {:?}", r));
            }
        }

        let pending_bundle = self.flashbots.inner().send_bundle(&bundle).await?;
        let bundle_hash = pending_bundle.await?;
        Ok(bundle_hash)
    }

    pub async fn send_tx(&self, tx: Eip1559TransactionRequest) -> Result<TxHash> {
        // Sign the transaction to get raw bytes regardless of submission path.
        let raw = self.sign_tx(tx).await?;

        if let Some(ref private_url) = self.private_rpc_url {
            // ── Private relay submission (MEV protection) ────────────────────
            // POST raw tx to the private RPC only — never touches the public mempool.
            // Receipt polling is spawned as a background task so this method returns
            // immediately and the event loop continues scanning the next block.
            let raw_hex = format!("0x{}", hex::encode(raw.as_ref()));
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "method":  "eth_sendRawTransaction",
                "params":  [raw_hex],
                "id":      1
            });
            let relay_resp = self.http_client
                .post(private_url)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow!("private relay POST failed: {e}"))?
                .json::<serde_json::Value>()
                .await
                .map_err(|e| anyhow!("private relay response parse failed: {e}"))?;

            if let Some(err) = relay_resp.get("error") {
                return Err(anyhow!("private relay rejected tx: {err}"));
            }

            // Derive tx hash: from relay response first, then keccak256 fallback.
            let tx_hash: TxHash = relay_resp["result"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| ethers::utils::keccak256(raw.as_ref()).into());

            log::info!("[private relay] tx submitted: {:?}", tx_hash);

            // Spawn a background task to poll for the receipt.
            // send_tx returns immediately — the event loop is never blocked.
            let provider_clone = self.provider.clone();
            tokio::spawn(async move {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
                loop {
                    if std::time::Instant::now() >= deadline {
                        log::warn!("[private relay] tx {:?} not confirmed within 90 s", tx_hash);
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    if let Ok(Some(receipt)) = provider_clone.get_transaction_receipt(tx_hash).await {
                        if receipt.status == Some(U64::from(0u64)) {
                            log::warn!(
                                "[private relay] ❌ reverted on-chain (tx: {:?}, block: {:?})",
                                receipt.transaction_hash, receipt.block_number,
                            );
                        } else {
                            log::info!(
                                "[private relay] ✅ confirmed (tx: {:?}, block: {:?})",
                                receipt.transaction_hash, receipt.block_number,
                            );
                        }
                        return;
                    }
                }
            });

            // Return the hash immediately — scanning resumes on the next block.
            Ok(tx_hash)
        } else {
            // ── Standard public-RPC submission ───────────────────────────────
            let pending = self.provider.send_raw_transaction(raw).await?;
            let receipt = pending.await?.ok_or_else(|| anyhow!("Tx dropped"))?;
            // EIP-658: status = 0 → reverted.
            if receipt.status == Some(U64::from(0u64)) {
                return Err(anyhow!(
                    "execution reverted on-chain (tx: {:?}, block: {:?})",
                    receipt.transaction_hash,
                    receipt.block_number,
                ));
            }
            Ok(receipt.transaction_hash)
        }
    }

    pub async fn transfer_in_tx(
        &self,
        amount_in: U256,
        max_priority_fee_per_gas: U256,
        max_fee_per_gas: U256,
    ) -> Result<Eip1559TransactionRequest> {
        let common = self._common_fields().await?;
        let to = NameOrAddress::Address(self.bot.address());
        Ok(Eip1559TransactionRequest {
            to: Some(to),
            from: Some(common.0),
            data: Some(Bytes(bytes::Bytes::new())),
            value: Some(amount_in),
            chain_id: Some(common.2),
            max_priority_fee_per_gas: Some(max_priority_fee_per_gas),
            max_fee_per_gas: Some(max_fee_per_gas),
            gas: Some(U256::from(60000)),
            nonce: Some(common.1),
            access_list: AccessList::default(),
        })
    }

    pub async fn transfer_out_tx(
        &self,
        token: &str,
        max_priority_fee_per_gas: U256,
        max_fee_per_gas: U256,
    ) -> Result<Eip1559TransactionRequest> {
        let token_address = Address::from_str(token).unwrap();
        let calldata = self.bot.encode("recoverToken", (token_address,))?;

        let common = self._common_fields().await?;
        let to = NameOrAddress::Address(self.bot.address());
        Ok(Eip1559TransactionRequest {
            to: Some(to),
            from: Some(common.0),
            data: Some(calldata),
            value: Some(U256::zero()),
            chain_id: Some(common.2),
            max_priority_fee_per_gas: Some(max_priority_fee_per_gas),
            max_fee_per_gas: Some(max_fee_per_gas),
            gas: Some(U256::from(50000)),
            nonce: Some(common.1),
            access_list: AccessList::default(),
        })
    }

    pub async fn approve_tx(
        &self,
        router: &str,
        tokens: Vec<&str>,
        force: bool,
        max_priority_fee_per_gas: U256,
        max_fee_per_gas: U256,
    ) -> Result<Eip1559TransactionRequest> {
        let router_address = Address::from_str(router).unwrap();
        let token_addresses: Vec<Address> = tokens
            .iter()
            .map(|token| Address::from_str(token).unwrap())
            .collect();
        let calldata = self
            .bot
            .encode("approveRouter", (router_address, token_addresses, force))?;

        let token_cnt = tokens.len();
        let common = self._common_fields().await?;
        let to = NameOrAddress::Address(self.bot.address());
        Ok(Eip1559TransactionRequest {
            to: Some(to),
            from: Some(common.0),
            data: Some(calldata),
            value: Some(U256::zero()),
            chain_id: Some(common.2),
            max_priority_fee_per_gas: Some(max_priority_fee_per_gas),
            max_fee_per_gas: Some(max_fee_per_gas),
            gas: Some(U256::from(55000) * U256::from(token_cnt)),
            nonce: Some(common.1),
            access_list: AccessList::default(),
        })
    }

    pub async fn order_tx(
        &self,
        paths: Vec<PathParam>,
        amount_in: U256,
        flashloan: Flashloan,
        loan_from: Address,
        max_priority_fee_per_gas: U256,
        max_fee_per_gas: U256,
        min_out: U256,
    ) -> Result<Eip1559TransactionRequest> {
        let nhop = paths.len();

        // Dynamic gas limit based on hop count and pool types.
        // Base cost covers flash-loan setup, calldata, SLOAD/SSTORE overhead.
        // CL (types 1, 4) and LFJ (type 2) use iterative math on-chain → more gas.
        let base_gas: u64 = 350_000;
        // CL gas scales with tick crossings: ~125k base + ~30k per tick.
        // Estimate worst-case crossings from MAX_TICK_CROSSES (10).
        // Typical: 1-3 crossings = ~185-215k.  Budget for ~5 average.
        let per_hop_gas: u64 = paths.iter().map(|p| match p.pool_type {
            1 | 3 => 280_000,   // Algebra (1) / UniswapV3CL (3) — ~125k + 5*30k + margin
            4     => 280_000,   // LFJ Liquidity Book — bin traversal
            _     => 180_000,   // V2, Solidly Stable
        }).sum();
        // Add a 15% buffer to handle multi-tick edge cases
        let gas_limit = base_gas + per_hop_gas + (per_hop_gas / 7);

        let mut params = Vec::new();
        params.extend(vec![
            abi::Token::Uint(amount_in),
            abi::Token::Uint(U256::from(flashloan as u64)),
            abi::Token::Address(loan_from),
            abi::Token::Uint(min_out),
        ]);

        for i in 0..nhop {
            params.extend(paths[i].make_params());
        }

        let encoded = abi::encode(&params);
        let calldata = Bytes::from(encoded);

        let common = self._common_fields().await?;
        let to = NameOrAddress::Address(self.bot.address());
        Ok(Eip1559TransactionRequest {
            to: Some(to),
            from: Some(common.0),
            data: Some(calldata),
            value: Some(U256::zero()),
            chain_id: Some(common.2),
            max_priority_fee_per_gas: Some(max_priority_fee_per_gas),
            max_fee_per_gas: Some(max_fee_per_gas),
            gas: Some(U256::from(gas_limit)),
            nonce: Some(common.1),
            access_list: AccessList::default(),
        })
    }
}

#[cfg(test)]
mod bundler_tests {
    use super::*;
    use crate::constants::{GWEI, WEI};

    #[ignore = "requires .env with PRIVATE_KEY/SIGNING_KEY/BOT_ADDRESS set"]
    #[tokio::test]
    async fn bundler_test() {
        let bundler = Bundler::new().expect("bundler init failed");

        let _tx = bundler
            .transfer_in_tx(
                U256::from(5) * *WEI,
                U256::from(50) * *GWEI,
                U256::from(200) * *GWEI,
            )
            .await
            .unwrap();
        // let tx_hash = bundler.send_tx(tx).await?;
        // println!("{:?}", tx_hash);

        // AVAX addresses — update if changing chains
        let wavax  = "0xB31f66AA3C1e785363F0875A1B74E27b85FD66c7"; // WAVAX
        let usdc   = "0xB97EF9Ef8734C71904D8002F8b6Bc66Dd9c48a6E"; // USDC.e
        let router = "0x04E1dee021Cd12bBa022A72806441B43d8212Fec"; // Blackhole V2 router
        let aave_pool = "0x794a61358D6845594F94dc1DB02A252b5b4814aD"; // Aave V3 pool (AVAX)

        let _tx = bundler
            .transfer_out_tx(
                wavax,
                U256::from(50) * *GWEI,
                U256::from(200) * *GWEI,
            )
            .await
            .unwrap();

        let _tx = bundler
            .approve_tx(
                router,
                vec![wavax, usdc],
                true,
                U256::from(50) * *GWEI,
                U256::from(200) * *GWEI,
            )
            .await
            .unwrap();

        let paths = vec![PathParam {
            router: Address::from_str(router).unwrap(),
            token_in: Address::from_str(wavax).unwrap(),
            token_out: Address::from_str(usdc).unwrap(),
            pool_type: 0,
            fee: 71,  // Blackhole V2
        }];
        let _tx = bundler
            .order_tx(
                paths,
                U256::from(1) * *WEI,
                Flashloan::AaveV3,
                Address::from_str(aave_pool).unwrap(),
                U256::from(100) * *GWEI,
                U256::from(300) * *GWEI,
                U256::zero(), // min_out (0 = no slippage check for test)
            )
            .await
            .unwrap();
    }
}

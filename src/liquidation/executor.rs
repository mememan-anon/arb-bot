/// AAVE v3 liquidation executor — Phase 3 of the liquidation sub-system.
///
/// Ported from overlord-rs/crates/profito-rs.
///
/// Architecture:
///   - Receives UnderwaterUserAlert from the health_factor broadcast channel.
///   - For each alert: fetches user reserve positions, queries oracle prices,
///     selects the most profitable (collateral, debt) pair using AAVE v3 close-
///     factor math, discovers the Uniswap V3 pools needed for the two swap legs,
///     and calls V2ArbBot.triggerLiquidation() if the expected net profit
///     exceeds the configured minimum.
///
/// Key differences from profito-rs:
///   - No ZMQ: receives alerts via tokio broadcast channel.
///   - No Foxdie / MEV-Share: submits directly via V2ArbBot.triggerLiquidation()
///     backed by a Balancer flash loan.
///   - tx signing uses a local private key loaded from EXECUTOR_PK env var.

use alloy::{
    network::EthereumWallet,
    primitives::{Address, FixedBytes, U256},
    providers::{Provider, ProviderBuilder, WsConnect},
    signers::local::PrivateKeySigner,
    sol,
};
use anyhow::{bail, Context, Result};
use log::{debug, error, info, warn};
use std::{collections::HashMap, io::Write};
use tokio::sync::broadcast;

use crate::config::LiquidationConfig;
use super::types::UnderwaterUserAlert;

// ── Inline ABI bindings ───────────────────────────────────────────────────────

sol! {
    /// One user position returned by UiPoolDataProvider.getUserReservesData.
    struct UserReserveData {
        address underlyingAsset;
        uint256 scaledATokenBalance;
        bool    usageAsCollateralEnabledOnUser;
        uint256 scaledVariableDebt;
    }

    /// Subset of the AAVE v3 UiPoolDataProvider ABI.
    #[sol(rpc)]
    interface IUiPoolDataProvider {
        function getUserReservesData(
            address poolAddressesProvider,
            address user
        ) external view returns (UserReserveData[] memory, uint8 userEmodeCategoryId);
    }
}

sol! {
    /// AAVE v3 Oracle — USD prices (8 decimals) per whole token.
    #[sol(rpc)]
    interface IAaveOracle {
        function getAssetsPrices(address[] calldata assets)
            external view returns (uint256[] memory);
    }
}

sol! {
    /// ERC-20 decimals and balance — needed for oracle prices and Morpho balance check.
    #[sol(rpc)]
    interface IERC20Min {
        function decimals() external view returns (uint8);
        function balanceOf(address account) external view returns (uint256);
    }
}

sol! {
    /// Uniswap V3 Factory pool lookup.
    #[sol(rpc)]
    interface IUniswapV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee)
            external view returns (address);
    }
}

sol! {
    /// Balancer V2 Vault — getPoolTokens used to verify a pool contains both tokens.
    #[sol(rpc)]
    interface IBalancerVault {
        function getPoolTokens(bytes32 poolId)
            external view returns (
                address[] memory tokens,
                uint256[] memory balances,
                uint256 lastChangeBlock
            );
    }
}

sol! {
    /// LiquidationParams must match V2ArbBot.sol struct field-for-field.
    struct LiquidationParams {
        address user;
        address collateralAsset;
        address debtAsset;
        uint256 debtToCover;
        address collateralPool;   // V3 pool: collateral→WETH (address(0) if collateral==WETH)
        address debtPool;         // V3 pool: WETH→debt       (address(0) if debt==WETH)
        bytes32 colBalancerPool;  // Balancer poolId collateral→WETH (bytes32(0) → use V3)
        bytes32 debtBalancerPool; // Balancer poolId WETH→debt       (bytes32(0) → use V3)
    }

    /// V2ArbBot entry points for flash-loan-backed AAVE v3 liquidations.
    #[sol(rpc)]
    interface IV2ArbBot {
        /// Flash-loan via Balancer Vault (fee typically 0% on Base).
        function triggerLiquidation(
            address balancerVault,
            LiquidationParams calldata p
        ) external;

        /// Flash-loan via Morpho Blue (fee = 0%, broad token coverage).
        /// swapVault: Balancer vault for swap legs; address(0) = V3-only swaps.
        function triggerLiquidationWithMorpho(
            address morpho,
            address swapVault,
            LiquidationParams calldata p
        ) external;

        /// Flash-loan via AAVE V3 (0.05% fee) — last-resort fallback.
        /// Uses the aavePool address already set in the bot contract.
        /// swapVault: Balancer vault for swap legs; address(0) = V3-only swaps.
        function triggerLiquidationWithAave(
            address swapVault,
            LiquidationParams calldata p
        ) external;
    }
}

// ── AAVE v3 math constants (ported from profito-rs/calculations.rs) ───────────

/// WAD = 1e18 — health factor precision.
const WAD: u128 = 1_000_000_000_000_000_000_u128;

/// AAVE trigger HF: if HF >= 0.95×WAD the close factor is 50%, else 100%.
const MAXIMUM_LIQUIDATION_HF: u128 = 950_000_000_000_000_000_u128; // 0.95e18

/// AAVE default 50% close factor in basis points.
const DEFAULT_CLOSE_FACTOR_BPS: u128 = 5_000_u128;
/// Full 100% close factor (when HF < 0.95).
const MAX_CLOSE_FACTOR_BPS: u128 = 10_000_u128;
const BPS_BASE: u128 = 10_000_u128;

/// Conservative liquidation bonus used for pair selection: 5%.
const APPROX_BONUS_BPS: u128 = 500_u128;

/// AAVE oracle prices have 8-decimal precision (same as USD).
const ORACLE_DEC: u32 = 8;

// ── Public entry point ────────────────────────────────────────────────────────

/// Liquidation executor task.
///
/// # Arguments
/// * `wss_url`        — WebSocket RPC URL (same as used by other tasks).
/// * `pap_addr`       — AAVE PoolAddressesProvider address (string).
/// * `ui_addr`        — AAVE UiPoolDataProvider address (string).
/// * `oracle_addr`    — AAVE Oracle address (string).
/// * `v3_factory`     — Uniswap V3 Factory address for discovering swap pools.
/// * `weth`           — WETH address on this chain.
/// * `bot_addr`       — Deployed V2ArbBot contract address.
/// * `balancer_vault` — Balancer Vault address (flash loan provider).
/// * `exec_cfg`       — Liquidation settings (profit floor, gas estimate, pools).
/// * `alert_rx`       — Broadcast receiver for UnderwaterUserAlert events.
/// * `private_tx_url`  — If set, liquidation txs are submitted via this separate WSS
///                       endpoint instead of the main provider (MEV protection).
pub async fn run(
    wss_url: String,
    pap_addr: String,
    ui_addr: String,
    oracle_addr: String,
    v3_factory: String,
    weth: String,
    bot_addr: String,
    balancer_vault: String,
    morpho_addr: Option<String>,
    aave_pool: Option<String>,
    exec_cfg: LiquidationConfig,
    mut alert_rx: broadcast::Receiver<UnderwaterUserAlert>,
    private_tx_url: Option<String>,
) -> Result<()> {
    // ── Validate / parse addresses ────────────────────────────────────────────
    let pap_addr:       Address = pap_addr.parse().context("bad pap_addr")?;
    let ui_addr:        Address = ui_addr.parse().context("bad ui_addr")?;
    let oracle_addr:    Address = oracle_addr.parse().context("bad oracle_addr")?;
    let v3_factory:     Address = v3_factory.parse().context("bad v3_factory")?;
    let weth:           Address = weth.parse().context("bad weth")?;
    let bot_addr:       Address = bot_addr.parse().context("bad bot_addr")?;
    let balancer_vault: Address = balancer_vault.parse().context("bad balancer_vault")?;    let morpho_addr: Option<Address> = morpho_addr
        .as_deref()
        .map(|s| s.parse::<Address>().context("bad morpho_addr"))
        .transpose()?;
    let aave_pool_addr: Option<Address> = aave_pool
        .as_deref()
        .map(|s| s.parse::<Address>().context("bad aave_pool"))
        .transpose()?;

    // ── Prepare opportunity log file ────────────────────────────────────
    let log_path = exec_cfg
        .opportunity_log
        .clone()
        .unwrap_or_else(|| "cache/base/liquidation_opportunities.csv".to_owned());
    if let Some(parent) = std::path::Path::new(&log_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Write CSV header once if the file does not yet exist.
    if !std::path::Path::new(&log_path).exists() {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true).append(true).open(&log_path)
        {
            let _ = writeln!(
                f,
                "timestamp_utc,trace_id,user,collateral_asset,debt_asset,\
                 debt_to_cover_usd,gross_profit_usd,gas_cost_usd,net_profit_usd,\
                 above_threshold,executed,tx_hash,flash_loan_source"
            );
        }
    }
    // ── Load private key from environment ─────────────────────────────────────
    // Reads PRIVATE_KEY from .env (same key used by the arb engine & forge scripts).
    let pk = std::env::var("PRIVATE_KEY")
        .or_else(|_| std::env::var("SIGNING_KEY"))  // legacy alias
        .context("PRIVATE_KEY env var not set — cannot submit liquidation txs")?;
    let signer: PrivateKeySigner = pk.parse().context("bad EXECUTOR_PK")?;
    let wallet = EthereumWallet::from(signer.clone());
    let executor_addr = signer.address();

    // ── Connect provider ──────────────────────────────────────────────────────
    // WSS provider is used for all read calls (eth_call, subscriptions).
    // TX submission goes through private_tx_url (HTTP) when configured,
    // which keeps the liquidation tx out of the public mempool.
    let ws = WsConnect::new(wss_url.clone());
    let provider = ProviderBuilder::new()
        .wallet(wallet.clone())
        .connect_ws(ws)
        .await
        .context("executor: WS connect failed")?;
    info!(
        "[executor] started wallet={executor_addr} bot={bot_addr} \
         private_tx={} morpho={} aave={}",
        private_tx_url.as_deref().unwrap_or("(public WSS)"),
        morpho_addr.map(|a| a.to_string()).as_deref().unwrap_or("disabled"),
        aave_pool_addr.map(|a| a.to_string()).as_deref().unwrap_or("disabled"),
    );

    // ── Alert processing loop ─────────────────────────────────────────────────
    loop {
        let alert = match alert_rx.recv().await {
            Ok(a) => a,
            Err(broadcast::error::RecvError::Closed) => {
                info!("[executor] alert channel closed — exiting");
                break;
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("[executor] lagged {n} alerts — some opportunities may have been missed");
                continue;
            }
        };

        if !exec_cfg.enabled {
            debug!(
                "[executor] disabled — skipped alert user={} trace={}",
                alert.user, alert.trace_id
            );
            continue;
        }

        let trace = alert.trace_id.clone();
        if let Err(e) = handle_alert(
            &provider,
            &wallet,
            private_tx_url.as_deref(),
            &alert,
            pap_addr,
            ui_addr,
            oracle_addr,
            v3_factory,
            weth,
            bot_addr,
            balancer_vault,
            morpho_addr,
            aave_pool_addr,
            &log_path,
            &exec_cfg,
        )
        .await
        {
            error!("[executor] trace={trace} error: {e:#}");
        }
    }

    Ok(())
}

// ── Per-alert handler ─────────────────────────────────────────────────────────

async fn handle_alert<P: Provider>(
    provider: &P,
    wallet: &EthereumWallet,
    private_tx_url: Option<&str>,
    alert: &UnderwaterUserAlert,
    pap_addr: Address,
    ui_addr: Address,
    oracle_addr: Address,
    v3_factory: Address,
    weth: Address,
    bot_addr: Address,
    balancer_vault: Address,
    morpho_addr: Option<Address>,
    aave_pool_addr: Option<Address>,
    log_path: &str,
    exec_cfg: &LiquidationConfig,
) -> Result<()> {
    let trace = &alert.trace_id;

    // ── 1. Fetch user's reserve positions ────────────────────────────────────
    let ui = IUiPoolDataProvider::new(ui_addr, provider);
    let user_reserves = ui
        .getUserReservesData(pap_addr, alert.user)
        .call()
        .await
        .with_context(|| format!("trace={trace} getUserReservesData failed"))?
        ._0;

    // Separate into collateral positions and debt positions.
    let collaterals: Vec<Address> = user_reserves
        .iter()
        .filter(|r| r.scaledATokenBalance > U256::ZERO && r.usageAsCollateralEnabledOnUser)
        .map(|r| r.underlyingAsset)
        .collect();

    let debts: Vec<Address> = user_reserves
        .iter()
        .filter(|r| r.scaledVariableDebt > U256::ZERO)
        .map(|r| r.underlyingAsset)
        .collect();

    if collaterals.is_empty() || debts.is_empty() {
        debug!(
            "[executor] trace={trace} user={} no active positions (col={} debt={})",
            alert.user, collaterals.len(), debts.len()
        );
        return Ok(());
    }

    // ── 2. Fetch oracle prices + token decimals for all involved assets ───────
    let all_assets: Vec<Address> = collaterals
        .iter()
        .chain(debts.iter())
        .chain(std::iter::once(&weth))
        .copied()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let oracle = IAaveOracle::new(oracle_addr, provider);
    let prices_raw: Vec<U256> = oracle
        .getAssetsPrices(all_assets.clone())
        .call()
        .await
        .with_context(|| format!("trace={trace} getAssetsPrices failed"))?;

    let price_map: HashMap<Address, U256> = all_assets
        .iter()
        .copied()
        .zip(prices_raw)
        .collect();

    // ERC-20 decimals — needed to normalise oracle prices to USD amounts.
    let mut decimals_map: HashMap<Address, u8> = HashMap::new();
    for &asset in &all_assets {
        let d: u8 = IERC20Min::new(asset, provider)
            .decimals()
            .call()
            .await
            .unwrap_or(18);
        decimals_map.insert(asset, d);
    }

    // ── 3. Determine close factor from health factor ─────────────────────────
    let hf_u128 = u128::try_from(alert.health_factor).unwrap_or(u128::MAX);
    let close_factor_bps = if hf_u128 >= MAXIMUM_LIQUIDATION_HF {
        DEFAULT_CLOSE_FACTOR_BPS
    } else {
        MAX_CLOSE_FACTOR_BPS
    };

    // ── 4. Select the best (collateral, debt) pair ───────────────────────────
    // For each pair: estimate profit_usd = debtToCover_usd * approxBonous
    // Pick the pair with the highest expected gross profit.
    let mut best: Option<(Address, Address, U256, u128)> = None; // (collateral, debt, debtToCover, profit_usd_e8)

    for &debt_asset in &debts {
        let reserve = user_reserves
            .iter()
            .find(|r| r.underlyingAsset == debt_asset)
            .unwrap();

        let debt_price = *price_map.get(&debt_asset).unwrap_or(&U256::ZERO);
        let debt_dec   = *decimals_map.get(&debt_asset).unwrap_or(&18) as u32;
        if debt_price.is_zero() { continue; }

        let debt_atoms = reserve.scaledVariableDebt;
        if debt_atoms.is_zero() { continue; }

        // debtToCover = scaledBalance × closeFactor.
        // Note: scaledVariableDebt is divided by variableBorrowIndex to give the
        // current balance.  We skip the index multiplication here (index ≥ 1e27)
        // so this slightly understates the true debt — safe for pair selection.
        let debt_to_cover = debt_atoms * U256::from(close_factor_bps) / U256::from(BPS_BASE);

        // usd_e8 = amount_atoms × price_e8 / 10^decimals
        let scale = U256::from(10_u128.pow(debt_dec));
        let debt_to_cover_usd_e8 = u128::try_from(debt_to_cover * debt_price / scale)
            .unwrap_or(u128::MAX);

        // Cap to max_position_size.
        let max_usd_e8 = (exec_cfg.max_position_size * 10_f64.powi(ORACLE_DEC as i32)) as u128;
        let (debt_to_cover, debt_to_cover_usd_e8) = if debt_to_cover_usd_e8 > max_usd_e8 {
            let capped = U256::from(max_usd_e8) * scale / debt_price;
            (capped, max_usd_e8)
        } else {
            (debt_to_cover, debt_to_cover_usd_e8)
        };

        for &col_asset in &collaterals {
            if col_asset == debt_asset { continue; }

            let gross = debt_to_cover_usd_e8.saturating_mul(APPROX_BONUS_BPS) / BPS_BASE;
            if let Some((_, _, _, prev)) = &best {
                if gross <= *prev { continue; }
            }
            best = Some((col_asset, debt_asset, debt_to_cover, gross));
        }
    }

    let (col_asset, debt_asset, debt_to_cover, gross_profit_e8) = match best {
        Some(b) => b,
        None => {
            info!("[executor] trace={trace} no viable pair found");
            return Ok(());
        }
    };

    // ── 5. Gas cost in USD (oracle units) ─────────────────────────────────────
    let eth_price_e8 = price_map
        .get(&weth)
        .copied()
        .unwrap_or(U256::from(2_000_u64 * 10_u64.pow(ORACLE_DEC))); // fallback $2000

    let gas_price_wei: u128 = provider
        .get_gas_price()
        .await
        .unwrap_or(1_000_000_000_u128); // 1 gwei fallback

    // gas_cost_usd_e8 = gasUnits × gasPrice(wei) × ethPrice(e8) / 1e18
    let gas_cost_e8 = u128::try_from(
        U256::from(exec_cfg.estimated_gas)
            * U256::from(gas_price_wei)
            * eth_price_e8
            / U256::from(WAD),
    )
    .unwrap_or(u128::MAX);

    let net_profit_e8 = gross_profit_e8.saturating_sub(gas_cost_e8);
    // Use the liquidation-specific profit floor (separate from arb min_profit_threshold).
    let liq_min_usd  = exec_cfg.min_liquidation_profit_usd;
    let min_profit_e8 = (liq_min_usd * 10_f64.powi(ORACLE_DEC as i32)) as u128;

    debug!(
        "[executor] trace={trace} user={} col={col_asset} debt={debt_asset} \
         gross_usd={:.4} gas_usd={:.4} net_usd={:.4} min_liq_usd={:.4}",
        alert.user,
        gross_profit_e8 as f64 / 10_f64.powi(ORACLE_DEC as i32),
        gas_cost_e8 as f64 / 10_f64.powi(ORACLE_DEC as i32),
        net_profit_e8 as f64 / 10_f64.powi(ORACLE_DEC as i32),
        liq_min_usd,
    );

    if net_profit_e8 < min_profit_e8 {
        info!(
            "[executor] trace={trace} UNPROFITABLE user={} net_usd={:.4} < min_liq={:.4}",
            alert.user,
            net_profit_e8 as f64 / 10_f64.powi(ORACLE_DEC as i32),
            liq_min_usd,
        );
        return Ok(());
    }

    // ── 6. Discover V3 swap pools ─────────────────────────────────────────────
    const FEE_TIERS: &[u32] = &[100, 500, 3000, 10000];

    let col_pool = if col_asset == weth {
        Address::ZERO
    } else {
        find_best_v3_pool(provider, v3_factory, col_asset, weth, FEE_TIERS).await?
    };
    let debt_pool = if debt_asset == weth {
        Address::ZERO
    } else {
        find_best_v3_pool(provider, v3_factory, weth, debt_asset, FEE_TIERS).await?
    };

    if col_pool.is_zero() && col_asset != weth {
        bail!("trace={trace} no V3 pool for collateral {col_asset} ↔ WETH");
    }
    if debt_pool.is_zero() && debt_asset != weth {
        bail!("trace={trace} no V3 pool for WETH ↔ debt {debt_asset}");
    }

    // \u2500\u2500 7. Look up Balancer pool IDs \u2014 prefer Balancer (0% fee) over Uniswap V3. \u2500\u2500─
    // A non-zero poolId tells the contract to swap through Balancer;
    // FixedBytes::ZERO means no Balancer pool found, fall back to the V3 address.
    let col_balancer = find_balancer_pool(provider, balancer_vault, col_asset, weth, &exec_cfg.balancer_pool_ids).await
        .unwrap_or(FixedBytes::ZERO);
    let debt_balancer = find_balancer_pool(provider, balancer_vault, weth, debt_asset, &exec_cfg.balancer_pool_ids).await
        .unwrap_or(FixedBytes::ZERO);

    // Only hard-fail when nothing at all was found for a required leg.
    if col_pool.is_zero() && col_balancer.is_zero() && col_asset != weth {
        bail!("trace={trace} no swap route (V3 or Balancer) for col {col_asset} ↔ WETH");
    }
    if debt_pool.is_zero() && debt_balancer.is_zero() && debt_asset != weth {
        bail!("trace={trace} no swap route (V3 or Balancer) for WETH ↔ debt {debt_asset}");
    }

    // \u2500\u2500 8. Submit triggerLiquidation \u2500─────────────────────────────────────────────
    // ── 8. Select flash-loan source (Morpho first; Balancer fallback) ────────────
    // Morpho Blue on Base charges 0% fee.  Prefer it when it holds enough of
    // the debt token; otherwise fall back to Balancer (also 0% on Base).
    // Tier 1 — Morpho Blue (0% fee)
    let morpho_ok = if let Some(morpho) = morpho_addr {
        IERC20Min::new(debt_asset, provider)
            .balanceOf(morpho)
            .call()
            .await
            .map(|b| b >= debt_to_cover)
            .unwrap_or(false)
    } else { false };

    // Tier 2 — Balancer Vault (0% fee)
    let balancer_ok = if !morpho_ok {
        IERC20Min::new(debt_asset, provider)
            .balanceOf(balancer_vault)
            .call()
            .await
            .map(|b| b >= debt_to_cover)
            .unwrap_or(false)
    } else { false };

    // Tier 3 — AAVE V3 (0.05% fee): only when neither Morpho nor Balancer can fund.
    let use_aave = !morpho_ok && !balancer_ok && aave_pool_addr.is_some();

    // When AAVE is the only option, verify the 0.05% premium still leaves a profit.
    if use_aave {
        // premium = 0.05% of flash-loaned amount.
        // debt_to_cover_usd_e8 = gross_profit_e8 × BPS_BASE / APPROX_BONUS_BPS
        let aave_premium_e8 = gross_profit_e8
            .saturating_mul(BPS_BASE)
            / APPROX_BONUS_BPS
            * 5
            / 10_000;
        let net_after_aave = net_profit_e8.saturating_sub(aave_premium_e8);
        if net_after_aave < min_profit_e8 {
            info!(
                "[executor] trace={trace} UNPROFITABLE via AAVE (0.05% premium erases margin) \
                 net_usd={:.4} aave_fee_usd={:.4} min_usd={liq_min_usd:.4}",
                net_profit_e8   as f64 / 10_f64.powi(ORACLE_DEC as i32),
                aave_premium_e8 as f64 / 10_f64.powi(ORACLE_DEC as i32),
            );
            return Ok(());
        }
    }

    let flash_source = if morpho_ok { "Morpho" } else if use_aave { "Aave" } else { "Balancer" };
    info!("[executor] trace={trace} flash_source={flash_source}");

    // ── 8a. Log opportunity to CSV ───────────────────────────────────────────
    {
        let scale   = 10_f64.powi(ORACLE_DEC as i32);
        let dtc_usd = gross_profit_e8 as f64 / scale
                      * (BPS_BASE as f64 / APPROX_BONUS_BPS as f64);
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true).append(true).open(log_path)
        {
            let _ = writeln!(
                f,
                "{ts},{trace},{user},{col_asset},{debt_asset},\
                 {dtc:.4},{gross:.4},{gas:.4},{net:.4},true,pending,,{flash_source}",
                user  = alert.user,
                dtc   = dtc_usd,
                gross = gross_profit_e8 as f64 / scale,
                gas   = gas_cost_e8    as f64 / scale,
                net   = net_profit_e8  as f64 / scale,
            );
        }
    }

    // ── 8b. Build LiquidationParams ──────────────────────────────────────────
    let params = LiquidationParams {
        user:             alert.user,
        collateralAsset:  col_asset,
        debtAsset:        debt_asset,
        debtToCover:      debt_to_cover,
        collateralPool:   col_pool,
        debtPool:         debt_pool,
        colBalancerPool:  col_balancer,
        debtBalancerPool: debt_balancer,
    };

    // ── 8c. Submit tx — private WSS preferred, fall back to public ────────────
    //   Morpho   → triggerLiquidationWithMorpho(morpho, balancer_vault, params)
    //   Balancer → triggerLiquidation(balancer_vault, params)
    //   AAVE     → triggerLiquidationWithAave(balancer_vault, params)  [0.05%]
    macro_rules! send_liq {
        ($prov:expr) => {{
            let bot = IV2ArbBot::new(bot_addr, $prov);
            if morpho_ok {
                bot.triggerLiquidationWithMorpho(
                    morpho_addr.unwrap(), // safe: morpho_ok => Some
                    balancer_vault,
                    params.clone(),
                )
                .send().await
                .with_context(|| format!("trace={trace} triggerLiquidationWithMorpho send failed"))?
                .watch().await
                .with_context(|| format!("trace={trace} Morpho tx confirmation failed"))?
            } else if use_aave {
                bot.triggerLiquidationWithAave(balancer_vault, params.clone())
                    .send().await
                    .with_context(|| format!("trace={trace} triggerLiquidationWithAave send failed"))?
                    .watch().await
                    .with_context(|| format!("trace={trace} AAVE tx confirmation failed"))?
            } else {
                bot.triggerLiquidation(balancer_vault, params.clone())
                    .send().await
                    .with_context(|| format!("trace={trace} triggerLiquidation send failed"))?
                    .watch().await
                    .with_context(|| format!("trace={trace} Balancer tx confirmation failed"))?
            }
        }};
    }

    let tx_hash: alloy::primitives::TxHash = if let Some(tx_url) = private_tx_url {
        let tx_provider = ProviderBuilder::new()
            .wallet(wallet.clone())
            .connect_ws(WsConnect::new(tx_url))
            .await
            .with_context(|| format!("trace={trace} private WSS connect failed"))?;
        send_liq!(&tx_provider)
    } else {
        send_liq!(provider)
    };

    info!(
        "[executor] trace={trace} ✓ LIQUIDATED tx={tx_hash} user={} col={col_asset} \
         debt={debt_asset} net_usd≈{:.4} via={} source={flash_source}",
        alert.user,
        net_profit_e8 as f64 / 10_f64.powi(ORACLE_DEC as i32),
        if private_tx_url.is_some() { "private" } else { "public" },
    );

    Ok(())
}

// ── Helper ────────────────────────────────────────────────────────────────────

/// Returns the first (lowest-fee) existing Uniswap V3 pool for (tokenA, tokenB).
/// Returns `Address::ZERO` if no pool exists at any of the given fee tiers.
async fn find_best_v3_pool<P: Provider>(
    provider: &P,
    factory: Address,
    token_a: Address,
    token_b: Address,
    fee_tiers: &[u32],
) -> Result<Address> {
    let f = IUniswapV3Factory::new(factory, provider);
    for &fee in fee_tiers {
        // uint24 (Uint<24, 1>) — all standard fee tiers fit in u16.
        let fee_u24 = alloy::primitives::Uint::<24, 1>::from(fee as u16);
        let pool: Address = f
            .getPool(token_a, token_b, fee_u24)
            .call()
            .await
            .with_context(|| format!("getPool({token_a},{token_b},{fee}) failed"))?;
        if pool != Address::ZERO {
            return Ok(pool);
        }
    }
    Ok(Address::ZERO)
}

/// Queries the Balancer V2 Vault to find a poolId whose registered tokens include
/// both `token_a` and `token_b`.
///
/// Returns `Ok(FixedBytes::ZERO)` (not an error) when no matching Balancer pool is
/// found — the contract falls back to Uniswap V3 for that swap leg.
///
/// Checks only `config_pools` (from `liquidation.balancer_pool_ids` in the TOML).
async fn find_balancer_pool<P: Provider>(
    provider: &P,
    vault: Address,
    token_a: Address,
    token_b: Address,
    config_pools: &[String],
) -> Result<FixedBytes<32>> {
    let v = IBalancerVault::new(vault, provider);
    for pool_id_hex in config_pools.iter().map(|s| s.as_str()) {
        let Ok(pid) = <FixedBytes<32> as std::str::FromStr>::from_str(pool_id_hex) else {
            continue;
        };
        let Ok(res) = v.getPoolTokens(pid).call().await else {
            continue;
        };
        if res.tokens.contains(&token_a) && res.tokens.contains(&token_b) {
            return Ok(pid);
        }
    }
    Ok(FixedBytes::ZERO)
}

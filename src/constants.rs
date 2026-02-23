use ethers::{
    prelude::Lazy,
    types::{Address, H160, U256, U64},
};
use std::str::FromStr;

use crate::config::Config;

pub static WEI: Lazy<U256> = Lazy::new(|| U256::from(10).pow(U256::from(18)));
pub static GWEI: Lazy<U256> = Lazy::new(|| U256::from(10).pow(U256::from(9)));

pub static ZERO_ADDRESS: Lazy<Address> =
    Lazy::new(|| Address::from_str("0x0000000000000000000000000000000000000000").unwrap());

pub fn get_env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("{key} is not set in environment"))
}

/// Returns Some(value) if the env var is set and non-empty, otherwise None.
pub fn get_env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

#[derive(Debug, Clone)]
pub struct Env {
    pub https_url: String,
    pub wss_url: String,
    pub chain_id: U64,
    /// None when PRIVATE_KEY is unset — bot runs in scan-only mode.
    pub private_key: Option<String>,
    /// None when SIGNING_KEY is unset — bot runs in scan-only mode.
    pub signing_key: Option<String>,
    /// None when BOT_ADDRESS is unset or empty — bot runs in scan-only mode.
    pub bot_address: Option<String>,
}

impl Env {
    pub fn from_config(cfg: &Config) -> Self {
        Env {
            https_url: cfg.chain.https_url.clone(),
            wss_url: cfg.chain.wss_url.clone(),
            chain_id: U64::from(cfg.chain.chain_id),
            private_key: get_env_opt("PRIVATE_KEY"),
            signing_key: get_env_opt("SIGNING_KEY"),
            bot_address: get_env_opt("BOT_ADDRESS"),
        }
    }
}

/// Parse the chain's blacklisted token addresses from config.
/// Tokens in this list are skipped during path generation.
pub fn get_blacklist_tokens(raw: &[String]) -> Vec<H160> {
    raw.iter()
        .filter_map(|addr| H160::from_str(addr).ok())
        .collect()
}

// Use later for broadcasting to multiple builders
// static BUILDER_URLS: &[&str] = &[
//     "https://builder0x69.io",
//     "https://rpc.beaverbuild.org",
//     "https://relay.flashbots.net",
//     "https://rsync-builder.xyz",
//     "https://rpc.titanbuilder.xyz",
//     "https://api.blocknative.com/v1/auction",
//     "https://mev.api.blxrbdn.com",
//     "https://eth-builder.com",
//     "https://builder.gmbit.co/rpc",
//     "https://buildai.net",
//     "https://rpc.payload.de",
//     "https://rpc.lightspeedbuilder.info",
//     "https://rpc.nfactorial.xyz",
// ];

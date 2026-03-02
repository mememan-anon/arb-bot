/// REVM-compatible database backed by a local reth Base node's MDBX store.
///
/// ## Purpose
/// Replaces RPC calls to `eth_getStorageAt` / `eth_getBalance` with direct
/// read-only MDBX access.  Latency drops from ~2-5ms per call to ~5-10μs.
/// This is the same technique used in the sandooo / BaseBuster family of bots.
///
/// ## Differences from BaseBuster's `HistoryDB`
/// | BaseBuster                        | This file                          |
/// |-----------------------------------|------------------------------------|
/// | `reth_node_ethereum::EthereumNode`| `reth_optimism_node::OpNode`        |
/// | `ChainSpecBuilder::mainnet()`     | `BASE_MAINNET.clone()`              |
/// | `reth` @ git HEAD (no tag)        | `reth` @ tag v1.0.8 (revm 14 match)|
///
/// ## Compile
/// ```bash
/// cargo build --features local-node
/// ```
///
/// ## Usage
/// ```rust,no_run
/// use rust::history_db::HistoryDB;
///
/// // Open data dir, load state at block 20_000_000
/// let db = HistoryDB::new("/data/reth/base", 20_000_000).unwrap();
///
/// // Use as a revm Database
/// // let evm = revm::EVM::new();
/// // evm.database(db);
/// ```

use alloy::primitives::{Address, StorageKey, B256, U256};
use eyre::Result;
use reth::api::NodeTypesWithDBAdapter;
use reth::providers::{
    providers::StaticFileProvider, AccountReader, BlockNumReader, DatabaseProviderFactory,
    ProviderFactory, StateProviderBox, StateProviderFactory,
};
use reth::utils::open_db_read_only;
use reth_db::{mdbx::DatabaseArguments, ClientVersion, DatabaseEnv};
use reth_optimism_chainspec::BASE_MAINNET;
use reth_optimism_node::OpNode;
use revm::primitives::{AccountInfo, Bytecode, KECCAK_EMPTY};
use revm::{Database, DatabaseRef};
use std::path::Path;
use std::sync::Arc;

/// Type alias for the Base-specific ProviderFactory.
type BaseProviderFactory = ProviderFactory<NodeTypesWithDBAdapter<OpNode, Arc<DatabaseEnv>>>;

/// REVM `Database` backed by a local Base reth node opened in read-only mode.
///
/// Created once at startup, then `set_block()` cheaply advances to any
/// historical or current block without re-opening the MDBX environment.
pub struct HistoryDB {
    /// Per-block state provider (replaced on each `set_block` call).
    db_provider: StateProviderBox,
    /// Factory kept alive to create new providers on `set_block`.
    provider_factory: BaseProviderFactory,
}

impl HistoryDB {
    /// Open the reth data directory for Base at the given block number.
    ///
    /// `db_path` is the reth data root, e.g. `/data/reth/base` or
    /// `$HOME/.local/share/reth/8453`.  It must contain:
    /// - `db/`           — MDBX database files
    /// - `static_files/` — reth static file segments
    pub fn new(db_path: &str, block: u64) -> Result<Self> {
        let path = Path::new(db_path);

        // Open in read-only mode — safe to run alongside a live node.
        let db = Arc::new(open_db_read_only(
            path.join("db").as_path(),
            DatabaseArguments::new(ClientVersion::default()),
        )?);

        // BASE_MAINNET is Arc<OpChainSpec> from reth-optimism-chainspec.
        // This replaces BaseBuster's `Arc::new(ChainSpecBuilder::mainnet().build())`.
        let spec = BASE_MAINNET.clone();

        let static_files = StaticFileProvider::read_only(path.join("static_files"), true)?;

        let provider_factory =
            BaseProviderFactory::new(db.clone(), spec, static_files);

        let db_provider = provider_factory
            .history_by_block_number(block)
            .map_err(|e| eyre::eyre!("Failed to create state provider for block {block}: {e}"))?;

        Ok(Self {
            db_provider,
            provider_factory,
        })
    }

    /// Advance the state view to a different block.
    ///
    /// Cheap — reuses the open MDBX environment, just moves the provider cursor.
    pub fn set_block(&mut self, block: u64) -> Result<()> {
        self.db_provider = self
            .provider_factory
            .history_by_block_number(block)
            .map_err(|e| eyre::eyre!("Failed to advance to block {block}: {e}"))?;
        Ok(())
    }

    /// Latest block number available in the local database.
    pub fn best_block(&self) -> Result<u64> {
        Ok(self.provider_factory.best_block_number()?)
    }
}

// ── revm::Database (mutable) ─────────────────────────────────────────────────

impl Database for HistoryDB {
    type Error = eyre::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        DatabaseRef::basic_ref(self, address)
    }

    fn code_by_hash(&mut self, _: B256) -> Result<Bytecode, Self::Error> {
        // revm calls this only when code_hash != KECCAK_EMPTY and the code is
        // not already loaded.  basic_ref pre-loads bytecode, so this path
        // should never be hit.
        panic!("HistoryDB::code_by_hash — code should already be loaded via basic()");
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        DatabaseRef::storage_ref(self, address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        DatabaseRef::block_hash_ref(self, number)
    }
}

// ── revm::DatabaseRef (immutable) ────────────────────────────────────────────

impl DatabaseRef for HistoryDB {
    type Error = eyre::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        // Fetch raw account (balance + nonce).
        let account = self
            .db_provider
            .basic_account(&address)
            .unwrap_or_default()
            .unwrap_or_default();

        // Fetch bytecode — reth stores code separately by hash.
        let code = self.db_provider.account_code(&address).unwrap_or_default();

        let info = if let Some(bytecode) = code {
            AccountInfo::new(
                account.balance,
                account.nonce,
                bytecode.hash_slow(),
                Bytecode::new_raw(bytecode.original_bytes()),
            )
        } else {
            // EOA or empty account.
            AccountInfo::new(account.balance, account.nonce, KECCAK_EMPTY, Bytecode::new())
        };

        Ok(Some(info))
    }

    fn code_by_hash_ref(&self, _: B256) -> Result<Bytecode, Self::Error> {
        panic!("HistoryDB::code_by_hash_ref — code should already be loaded via basic_ref()");
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let value = self
            .db_provider
            .storage(address, StorageKey::from(index))?;
        Ok(value.unwrap_or_default())
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        let hash = self.db_provider.block_hash(number).unwrap_or_default();
        Ok(hash.map(|h| B256::new(h.0)).unwrap_or(KECCAK_EMPTY))
    }
}

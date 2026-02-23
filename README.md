# MEV template for Rustaceans

Running the Rust template can be done by running below:

```bash
cargo run
```

Make sure to have the .env file ready before you start running.

You can also checkout the speed performance of this system by running the benchmark functions:

```bash
cargo bench
```

# MEV bot contracts

Use Anvil mainnet hardforks to test the contracts.

```bash
anvil --fork-url $HTTPS_RPC_URL
```

# AMM volatile pools (Pharaoh, Blackhole, etc.) cached via: python scripts/pull_pharaoh.py --amm
# Run: python scripts/pull_pharaoh.py --amm --config-path config/avax.toml
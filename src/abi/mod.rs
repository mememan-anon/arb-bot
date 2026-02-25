use ethers_core::abi::Abi;
use std::fs;

pub struct ABI {
    pub erc20: Abi,
    pub weth: Abi,
    pub uniswap_v2_factory: Abi,
    pub uniswap_v2_pair: Abi,
    pub v2_arb_bot: Abi,
}

impl ABI {
    pub fn new() -> Self {
        let erc20_json = fs::read_to_string("src/abi/ERC20.json").expect("failed to read src/abi/ERC20.json");
        let weth_json = fs::read_to_string("src/abi/WETH.json").expect("failed to read src/abi/WETH.json");
        let uniswap_v2_factory_json = fs::read_to_string("src/abi/UniswapV2Factory.json").expect("failed to read src/abi/UniswapV2Factory.json");
        let uniswap_v2_pair_json = fs::read_to_string("src/abi/UniswapV2Pair.json").expect("failed to read src/abi/UniswapV2Pair.json");
        let v2_arb_bot_json = fs::read_to_string("src/abi/V2ArbBot.json").expect("failed to read src/abi/V2ArbBot.json");
        Self {
            erc20: serde_json::from_str(erc20_json.trim_start_matches('\u{FEFF}')).expect("failed to parse JSON from src/abi/ERC20.json"),
            weth: serde_json::from_str(weth_json.trim_start_matches('\u{FEFF}')).expect("failed to parse JSON from src/abi/WETH.json"),
            uniswap_v2_factory: serde_json::from_str(uniswap_v2_factory_json.trim_start_matches('\u{FEFF}')).expect("failed to parse JSON from src/abi/UniswapV2Factory.json"),
            uniswap_v2_pair: serde_json::from_str(uniswap_v2_pair_json.trim_start_matches('\u{FEFF}')).expect("failed to parse JSON from src/abi/UniswapV2Pair.json"),
            v2_arb_bot: serde_json::from_str(v2_arb_bot_json.trim_start_matches('\u{FEFF}')).expect("failed to parse JSON from src/abi/V2ArbBot.json"),
        }
    }
}

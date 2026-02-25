// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import "forge-std/Script.sol";
import "../src/V2ArbBot.sol";

/// @notice Deploy V2ArbBot and run all required post-deploy setup in one broadcast.
///
/// Chain-agnostic: all addresses are read from environment variables so the
/// same script works on Base, Avalanche, or any other supported chain.
///
/// Required env vars:
///   DEPLOY_WETH       — wrapped native token address (e.g. WETH on Base, WAVAX on Avax)
///   DEPLOY_AAVE_V3    — Aave V3 Pool address
///   DEPLOY_FLASH_ASSET_0 .. DEPLOY_FLASH_ASSET_N  — comma-free list of tokens to whitelist
///
/// Convenience: set a .env per chain, e.g. .env.base / .env.avax, and source before running.
///
/// Usage:
///   source .env.base   # or .env.avax
///   forge script script/DeployV2ArbBot.s.sol \
///       --rpc-url $RPC_URL \
///       --private-key $PRIVATE_KEY \
///       --broadcast \
///       --verify \
///       --etherscan-api-key $SCAN_API_KEY
contract DeployV2ArbBot is Script {
    function run() external returns (V2ArbBot deployed) {
        // ── Read chain-specific addresses from env ────────────────────────────
        address weth    = vm.envAddress("DEPLOY_WETH");
        address aaveV3  = vm.envAddress("DEPLOY_AAVE_V3");

        // Flash-loan whitelist: up to 8 tokens, zero address = skip
        address[8] memory flashAssets;
        flashAssets[0] = vm.envOr("DEPLOY_FLASH_ASSET_0", address(0));
        flashAssets[1] = vm.envOr("DEPLOY_FLASH_ASSET_1", address(0));
        flashAssets[2] = vm.envOr("DEPLOY_FLASH_ASSET_2", address(0));
        flashAssets[3] = vm.envOr("DEPLOY_FLASH_ASSET_3", address(0));
        flashAssets[4] = vm.envOr("DEPLOY_FLASH_ASSET_4", address(0));
        flashAssets[5] = vm.envOr("DEPLOY_FLASH_ASSET_5", address(0));
        flashAssets[6] = vm.envOr("DEPLOY_FLASH_ASSET_6", address(0));
        flashAssets[7] = vm.envOr("DEPLOY_FLASH_ASSET_7", address(0));

        vm.startBroadcast();

        // 1. Deploy
        deployed = new V2ArbBot(
            msg.sender,  // owner = broadcaster
            weth
        );

        // 2. Trust the Aave V3 pool (required for executeOperation callback)
        deployed.setAavePool(aaveV3);

        // 3. Whitelist flash-loan assets (skip zero-address slots)
        for (uint256 i = 0; i < 8; i++) {
            if (flashAssets[i] != address(0)) {
                deployed.setAllowedFlashAsset(flashAssets[i], true);
            }
        }

        vm.stopBroadcast();

        console.log("Deployed V2ArbBot at:", address(deployed));
        console.log("Owner:               ", msg.sender);
        console.log("WETH (native):       ", weth);
        console.log("aavePool:            ", deployed.aavePool());
        for (uint256 i = 0; i < 8; i++) {
            if (flashAssets[i] != address(0)) {
                console.log("allowed:", flashAssets[i], deployed.allowedFlashAssets(flashAssets[i]));
            }
        }
        console.log("Next step: export BOT_ADDRESS=%s", address(deployed));
    }
}

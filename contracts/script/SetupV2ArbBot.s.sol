// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import "forge-std/Script.sol";
import "../src/V2ArbBot.sol";

/// @notice Post-deploy initialisation for a V2ArbBot instance.
///
/// Chain-agnostic: reads all addresses from env vars — no hardcoded chain addresses.
///
/// Required env vars:
///   BOT_ADDRESS       — deployed V2ArbBot contract address
///   DEPLOY_AAVE_V3    — Aave V3 Pool address
///   DEPLOY_FLASH_ASSET_0 .. DEPLOY_FLASH_ASSET_N  — tokens to whitelist
///
/// Usage:
///   source .env.base   # or .env.avax
///   export BOT_ADDRESS=0x…   # address printed by DeployV2ArbBot
///   forge script script/SetupV2ArbBot.s.sol \
///       --rpc-url $RPC_URL \
///       --private-key $PRIVATE_KEY \
///       --broadcast
contract SetupV2ArbBot is Script {
    function run() external {
        // ── Read from env — no hardcoded chain addresses ───────────────────
        address botAddr = vm.envAddress("BOT_ADDRESS");
        address aaveV3  = vm.envAddress("DEPLOY_AAVE_V3");

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

        V2ArbBot bot = V2ArbBot(payable(botAddr));

        // 1. Trust the Aave V3 pool so executeOperation() doesn't revert
        bot.setAavePool(aaveV3);
        console.log("setAavePool:", aaveV3);

        // 2. Whitelist flash-loan assets (skip zero-address slots)
        for (uint256 i = 0; i < 8; i++) {
            if (flashAssets[i] != address(0)) {
                bot.setAllowedFlashAsset(flashAssets[i], true);
                console.log("allowed:", flashAssets[i]);
            }
        }

        vm.stopBroadcast();

        // Sanity-print current state
        console.log("--- verification ---");
        console.log("bot         :", botAddr);
        console.log("aavePool    :", bot.aavePool());
        for (uint256 i = 0; i < 8; i++) {
            if (flashAssets[i] != address(0)) {
                console.log("allowed?    :", flashAssets[i], bot.allowedFlashAssets(flashAssets[i]));
            }
        }
    }
}

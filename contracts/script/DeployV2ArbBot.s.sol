// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import "forge-std/Script.sol";
import "../src/V2ArbBot.sol";

/// @notice Deploy V2ArbBot and print its address.
/// Usage (local fork):
///   forge script script/DeployV2ArbBot.s.sol \
///       --rpc-url http://127.0.0.1:8545 \
///       --private-key <KEY> \
///       --broadcast
/// After deploying, copy the address into BOT_ADDRESS in your .env file,
/// then call setAllowedFlashAsset(WAVAX, true) on the deployed contract.
contract DeployV2ArbBot is Script {
    function run() external returns (V2ArbBot deployed) {
        vm.startBroadcast();
        deployed = new V2ArbBot(
            msg.sender,                                          // owner = broadcaster
            0xB31f66AA3C1e785363F0875A1B74E27b85FD66c7           // WAVAX
        );
        vm.stopBroadcast();

        console.log("Deployed V2ArbBot at:", address(deployed));
        console.log("Owner:", msg.sender);
        console.log("Next step: set BOT_ADDRESS=%s in .env", address(deployed));
    }
}

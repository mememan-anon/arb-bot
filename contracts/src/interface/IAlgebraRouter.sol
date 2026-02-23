// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

interface IAlgebraRouter {
    // Blackhole/Algebra V3 uses a deployer field in ExactInputSingleParams
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        address deployer;        // pool deployer address (from router.poolDeployer())
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 limitSqrtPrice;
    }

    function exactInputSingle(ExactInputSingleParams calldata params)
        external
        payable
        returns (uint256 amountOut);

    function poolDeployer() external view returns (address);
}

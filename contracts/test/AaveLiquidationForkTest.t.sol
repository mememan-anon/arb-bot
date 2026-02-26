// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import "forge-std/Test.sol";
import "../src/V2ArbBot.sol";

// ── Minimal interfaces used only in the test ──────────────────────────────────

interface IFullAavePool {
    function supply(address asset, uint256 amount, address onBehalfOf, uint16 referralCode) external;
    function borrow(address asset, uint256 amount, uint256 interestRateMode, uint16 referralCode, address onBehalfOf) external;
    function getUserAccountData(address user) external view returns (
        uint256 totalCollateralBase,
        uint256 totalDebtBase,
        uint256 availableBorrowsBase,
        uint256 currentLiquidationThreshold,
        uint256 ltv,
        uint256 healthFactor
    );
}

interface IAaveOracle {
    function setAssetSources(address[] calldata assets, address[] calldata sources) external;
    function getAssetPrice(address asset) external view returns (uint256);
}

interface IACLManager {
    function addPoolAdmin(address admin) external;
}

interface IAaveAddressesProvider {
    function getACLManager() external view returns (address);
}

interface IWETH9 {
    function deposit() external payable;
    function approve(address spender, uint256 amount) external returns (bool);
    function balanceOf(address owner) external view returns (uint256);
}

interface IERC20Min {
    function balanceOf(address owner) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
}

// ── Mock Chainlink aggregator that returns a fixed price ──────────────────────

contract MockV3Aggregator {
    int256  private _answer;
    uint8   public  decimals = 8;
    uint256 public  version  = 4;

    constructor(int256 answer) { _answer = answer; }

    function latestAnswer() external view returns (int256) { return _answer; }

    function latestRoundData() external view returns (
        uint80  roundId,
        int256  answer,
        uint256 startedAt,
        uint256 updatedAt,
        uint80  answeredInRound
    ) {
        return (1, _answer, block.timestamp, block.timestamp, 1);
    }

    function description() external pure returns (string memory) { return "Mock/USD"; }
}

// ── AAVE V3 flash-loan fork test ──────────────────────────────────────────────
//
// Tests triggerLiquidationWithAave() — the last-resort path used when neither
// Morpho Blue nor Balancer hold enough of the debt token.
//
// AAVE V3 on Base charges a 0.05% flash-loan premium.
// The bot repays `amount + premium` inside executeOperation().
//
// Economics (same as Balancer/Morpho test):
//   Flash-borrow 10 500 USDC → liquidate testUser → receive ~10 WETH
//   Sell 10 WETH via Aerodrome CL V3 pool → USDC
//   Repay 10 500 × 1.0005 ≈ 10 505.25 USDC to AAVE
//   Surplus ≈ $15 724 (very slightly less than Morpho/Balancer due to 0.05% fee)

contract AaveLiquidationForkTest is Test {

    // ── Base-mainnet addresses ────────────────────────────────────────────────
    address constant WETH           = 0x4200000000000000000000000000000000000006;
    address constant USDC           = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant AAVE_POOL      = 0xA238Dd80C259a72e81d7e4664a9801593F98d1c5;
    address constant PAP            = 0xe20fCBdBfFC4Dd138cE8b2E6FBb6CB49777ad64D;
    address constant ORACLE         = 0x2Cc0Fc26eD4563A5ce5e8bdcfe1A2878676Ae156;
    address constant BAL_VAULT      = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    // Aerodrome CL WETH/USDC 0.05% — WETH is token0, zeroForOne=true for WETH→USDC
    address constant WETH_USDC_V3   = 0xd0b53D9277642d899DF5C87A3966A349A798F224;
    address constant ACL_ADMIN      = 0x9390B1735def18560c509E2d0bc090E9d6BA257a;

    V2ArbBot bot;
    address  testUser;

    // ── setUp ─────────────────────────────────────────────────────────────────
    function setUp() public {
        vm.createSelectFork(vm.envOr("FORK_URL", string("https://mainnet.base.org")));

        bot = new V2ArbBot(address(this), WETH);
        bot.setAavePool(AAVE_POOL);
        bot.setAllowedFlashAsset(USDC, true);
        bot.setAllowedFlashAsset(WETH, true);

        testUser = makeAddr("testUser");
        vm.deal(testUser, 11 ether);

        vm.startPrank(testUser);
        IWETH9(WETH).deposit{value: 10 ether}();
        IERC20Min(WETH).approve(AAVE_POOL, type(uint256).max);
        IFullAavePool(AAVE_POOL).supply(WETH, 10e18, testUser, 0);
        IFullAavePool(AAVE_POOL).borrow(USDC, 10_000e6, 2, 0, testUser);
        vm.stopPrank();

        (, , , , , uint256 hf) = IFullAavePool(AAVE_POOL).getUserAccountData(testUser);
        emit log_named_decimal_uint("initial HF (e18)", hf, 18);
        assertGt(hf, 1e18, "setUp: position should be healthy");
    }

    // ── Main test: crash oracle, execute via AAVE flash loan ──────────────────
    function test_detectAndLiquidateViaAave() public {

        // ── Step 1: Confirm AAVE V3 pool is live and accepting flash loan requests ─
        // AAVE V3.1 on Base uses virtual-accounting mode: the pool's ERC-20 balance
        // does NOT reflect flash-loan capacity.  We simply verify the contract exists
        // (non-zero code) and trust AAVE's internal liquidity management.
        uint256 codeLen;
        assembly { codeLen := extcodesize(AAVE_POOL) }
        assertGt(codeLen, 0, "AAVE pool contract must exist at the configured address");

        // ── Step 2: Crash WETH oracle to $500 ────────────────────────────────
        MockV3Aggregator crashedOracle = new MockV3Aggregator(500e8);

        address aclManager = IAaveAddressesProvider(PAP).getACLManager();
        vm.prank(ACL_ADMIN);
        IACLManager(aclManager).addPoolAdmin(address(this));

        address[] memory assets  = new address[](1);
        address[] memory sources = new address[](1);
        assets[0]  = WETH;
        sources[0] = address(crashedOracle);
        IAaveOracle(ORACLE).setAssetSources(assets, sources);

        uint256 newPrice = IAaveOracle(ORACLE).getAssetPrice(WETH);
        emit log_named_decimal_uint("WETH oracle price (e8)", newPrice, 8);
        assertEq(newPrice, 500e8, "oracle price should be $500");

        // ── Step 3: Execute liquidation via AAVE flash loan ───────────────────
        // Strategy:
        //   • triggerLiquidationWithAave(BAL_VAULT, params) — uses aavePool stored
        //     in the contract; fires flashLoanSimple for 10 500 USDC
        //   • executeOperation callback: call _executeLiquidationCore, then
        //     approve(aavePool, 10_500e6 + premium) for repayment
        //   • AAVE premium = 10_500e6 × 0.05% ≈ 5.25 USDC
        V2ArbBot.LiquidationParams memory p = V2ArbBot.LiquidationParams({
            user:             testUser,
            collateralAsset:  WETH,
            debtAsset:        USDC,
            debtToCover:      10_500e6,
            collateralPool:   address(0),     // collateral IS WETH
            debtPool:         WETH_USDC_V3,   // WETH→USDC via Aerodrome CL V3
            colBalancerPool:  bytes32(0),
            debtBalancerPool: bytes32(0)
        });

        uint256 botUsdcBefore = IERC20Min(USDC).balanceOf(address(bot));
        uint256 botWethBefore = IERC20Min(WETH).balanceOf(address(bot));

        // swapVault = BAL_VAULT so Balancer swap legs remain available if needed.
        bot.triggerLiquidationWithAave(BAL_VAULT, p);

        uint256 botUsdcAfter = IERC20Min(USDC).balanceOf(address(bot));
        uint256 botWethAfter = IERC20Min(WETH).balanceOf(address(bot));

        uint256 usdcProfit = botUsdcAfter - botUsdcBefore;
        uint256 wethProfit = botWethAfter - botWethBefore;

        emit log_named_decimal_uint("Bot USDC profit (AAVE)", usdcProfit, 6);
        emit log_named_decimal_uint("Bot WETH profit (AAVE)", wethProfit, 18);

        // ── Step 4: Assertions ────────────────────────────────────────────────
        bool profitable = (usdcProfit > 0) || (wethProfit > 0);
        assertTrue(profitable, "AAVE liquidation should yield positive profit");

        // Expect at least $5 000 USDC profit even after the 0.05% AAVE fee
        // (10 WETH at real ~$2 000 minus 10 500 + ~5 USDC premium ≈ $9 495+)
        assertGt(usdcProfit, 5_000e6, "expected USDC profit > $5 000 via AAVE");

        // Confirm testUser's collateral was seized
        address aWETH = 0xD4a0e0b9149BCee3C920d2E00b5dE09138fd8bb7;
        uint256 testUserAWeth = IERC20Min(aWETH).balanceOf(testUser);
        emit log_named_decimal_uint("testUser aWETH remaining", testUserAWeth, 18);
        assertLt(testUserAWeth, 1e15, "testUser aWETH should be fully seized");
    }
}

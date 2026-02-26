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

// ── Fork test ─────────────────────────────────────────────────────────────────

contract LiquidationForkTest is Test {

    // ── Base-mainnet addresses ────────────────────────────────────────────────
    address constant WETH           = 0x4200000000000000000000000000000000000006;
    address constant USDC           = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
    address constant AAVE_POOL      = 0xA238Dd80C259a72e81d7e4664a9801593F98d1c5;
    address constant PAP            = 0xe20fCBdBfFC4Dd138cE8b2E6FBb6CB49777ad64D;
    address constant ORACLE         = 0x2Cc0Fc26eD4563A5ce5e8bdcfe1A2878676Ae156;
    address constant BAL_VAULT      = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    // Aerodrome CL WETH/USDC 0.05% — WETH is token0 (lower address) → zeroForOne=true for WETH→USDC
    address constant WETH_USDC_V3   = 0xd0b53D9277642d899DF5C87A3966A349A798F224;
    address constant ACL_ADMIN      = 0x9390B1735def18560c509E2d0bc090E9d6BA257a;

    V2ArbBot bot;
    address  testUser;

    // ── setUp: fork Base, deploy bot, seed a WETH/USDC AAVE position ─────────
    function setUp() public {
        // Fork Base mainnet — set RPC via env var or fall back to public endpoint
        vm.createSelectFork(vm.envOr("FORK_URL", string("https://mainnet.base.org")));

        // Deploy V2ArbBot — test contract is owner; WETH is mainCurrency
        bot = new V2ArbBot(address(this), WETH);
        bot.setAavePool(AAVE_POOL);
        bot.setAllowedFlashAsset(USDC, true);
        bot.setAllowedFlashAsset(WETH, true);

        // Create test user with a healthy WETH collateral / USDC borrow position
        testUser = makeAddr("testUser");
        vm.deal(testUser, 11 ether);

        vm.startPrank(testUser);

        // Wrap 10 ETH → WETH
        IWETH9(WETH).deposit{value: 10 ether}();
        IERC20Min(WETH).approve(AAVE_POOL, type(uint256).max);

        // Supply 10 WETH as AAVE collateral
        IFullAavePool(AAVE_POOL).supply(WETH, 10e18, testUser, 0);

        // Borrow 10 000 USDC at variable rate (mode 2).
        // Conservative: 10 WETH × $1 500 (floor) × 80% LTV = $12 000 max.
        // $10 000 is safely within LTV regardless of current WETH price.
        IFullAavePool(AAVE_POOL).borrow(USDC, 10_000e6, 2, 0, testUser);

        vm.stopPrank();

        // Sanity-check: confirm position is healthy
        (, , , , , uint256 hf) = IFullAavePool(AAVE_POOL).getUserAccountData(testUser);
        emit log_named_decimal_uint("initial HF (e18)", hf, 18);
        assertGt(hf, 1e18, "setUp: position should be healthy");
    }

    // ── Main test: crash oracle, detect & execute liquidation ─────────────────
    function test_detectAndLiquidate() public {

        // ── Step 1: Verify Balancer vault holds enough USDC for flash loan ────
        uint256 vaultUsdcBefore = IERC20Min(USDC).balanceOf(BAL_VAULT);
        emit log_named_decimal_uint("Balancer vault USDC balance", vaultUsdcBefore, 6);
        assertGe(vaultUsdcBefore, 10_500e6, "Balancer vault must hold >=10 500 USDC");

        // ── Step 2: Crash WETH oracle to $500 ────────────────────────────────
        MockV3Aggregator crashedOracle = new MockV3Aggregator(500e8); // $500 per WETH

        // Obtain ACL Manager from addresses provider
        address aclManager = IAaveAddressesProvider(PAP).getACLManager();

        // ACL Admin (DEFAULT_ADMIN) grants pool-admin role to this test contract
        vm.prank(ACL_ADMIN);
        IACLManager(aclManager).addPoolAdmin(address(this));

        // Set crashed price source for WETH
        address[] memory assets  = new address[](1);
        address[] memory sources = new address[](1);
        assets[0]  = WETH;
        sources[0] = address(crashedOracle);
        IAaveOracle(ORACLE).setAssetSources(assets, sources);

        // Verify oracle price changed
        uint256 newPrice = IAaveOracle(ORACLE).getAssetPrice(WETH);
        emit log_named_decimal_uint("WETH oracle price (e8)", newPrice, 8);
        assertEq(newPrice, 500e8, "oracle price should be $500");

        // ── Step 3: Confirm position is now underwater ────────────────────────
        // We skip getUserAccountData here (it reads through all reserves and can
        // hit a revm NotActivated path on some blocks when a custom oracle is set).
        // Correctness is proven implicitly: AAVE will revert liquidationCall with
        // HEALTH_FACTOR_NOT_BELOW_THRESHOLD if HF >= 1, so a successful execution
        // proves the position is underwater.

        // ── Step 4: Execute liquidation via V2ArbBot ──────────────────────────
        // Strategy:
        //   • Flash-borrow 10 500 USDC from Balancer vault (fee = 0 on Base)
        //   • liquidationCall → receive 10 WETH (full collateral; deep HF < 0.95)
        //   • Swap 10 WETH → USDC via Aerodrome CL V3 pool (WETH=token0, zeroForOne=true)
        //   • Repay 10 500 USDC; surplus stays in bot
        V2ArbBot.LiquidationParams memory p = V2ArbBot.LiquidationParams({
            user:             testUser,
            collateralAsset:  WETH,           // we receive WETH
            debtAsset:        USDC,           // we repay USDC
            debtToCover:      10_500e6,       // slight over-estimate; AAVE caps at actual debt
            collateralPool:   address(0),     // collateral IS WETH — no col→WETH swap needed
            debtPool:         WETH_USDC_V3,   // WETH→USDC via Aerodrome CL V3 (fallback V3 path)
            colBalancerPool:  bytes32(0),     // use V3 path, not Balancer for col swap
            debtBalancerPool: bytes32(0)      // use V3 path, not Balancer for debt swap
        });

        uint256 botUsdcBefore = IERC20Min(USDC).balanceOf(address(bot));
        uint256 botWethBefore = IERC20Min(WETH).balanceOf(address(bot));

        bot.triggerLiquidation(BAL_VAULT, p);

        uint256 botUsdcAfter = IERC20Min(USDC).balanceOf(address(bot));
        uint256 botWethAfter = IERC20Min(WETH).balanceOf(address(bot));

        uint256 usdcProfit = botUsdcAfter - botUsdcBefore;
        uint256 wethProfit = botWethAfter - botWethBefore;

        emit log_named_decimal_uint("Bot USDC profit", usdcProfit, 6);
        emit log_named_decimal_uint("Bot WETH profit", wethProfit, 18);

        // ── Step 5: Assertions ────────────────────────────────────────────────
        // Any positive balance increase = profitable liquidation
        bool profitable = (usdcProfit > 0) || (wethProfit > 0);
        assertTrue(profitable, "liquidation should yield a positive profit");

        // Expect at least $5 000 USDC profit
        // (10 WETH at real ~$2 000 minus 10 500 USDC repayment ≈ $9 500+)
        assertGt(usdcProfit, 5_000e6, "expected USDC profit > $5 000");

        // The testUser's collateral should have been seized (aWETH burned).
        // aWETH contract reported in setUp trace: 0xD4a0e0b9149BCee3C920d2E00b5dE09138fd8bb7
        address aWETH = 0xD4a0e0b9149BCee3C920d2E00b5dE09138fd8bb7;
        uint256 testUserAWeth = IERC20Min(aWETH).balanceOf(testUser);
        emit log_named_decimal_uint("testUser aWETH remaining", testUserAWeth, 18);
        // Full collateral seized — testUser's aWETH should be ~0 (dust allowed)
        assertLt(testUserAWeth, 1e15, "testUser aWETH should be fully seized (< 0.001 WETH dust)");
    }
}

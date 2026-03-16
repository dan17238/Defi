// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test, console2} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {FlashLiquidator} from "../src/FlashLiquidator.sol";
import {IAaveV3Pool, IFlashLoanSimpleReceiver} from "../src/interfaces/IAaveV3Pool.sol";
import {IRadiantLendingPool, IFlashLoanReceiver} from "../src/interfaces/IRadiantPool.sol";
import {SwapHelper} from "../src/libraries/SwapHelper.sol";

/// @dev Simple ERC20 mock for unit tests (no fork needed)
contract MockERC20 is ERC20 {
    uint8 private _decimals;

    constructor(string memory name, string memory symbol, uint8 dec) ERC20(name, symbol) {
        _decimals = dec;
    }

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    function decimals() public view override returns (uint8) {
        return _decimals;
    }
}

/// @title FlashLiquidatorTest
/// @notice Unit and integration tests for the FlashLiquidator contract
contract FlashLiquidatorTest is Test {
    FlashLiquidator public liquidator;
    MockERC20 public mockUSDC;
    MockERC20 public mockWETH;

    address public deployer;
    address public attacker = makeAddr("attacker");
    address public user = makeAddr("user");

    // Arbitrum mainnet token addresses (used in callback auth tests only)
    address constant WETH = 0x82aF49447D8a07e3bd95BD0d56f35241523fBab1;
    address constant USDC = 0xaf88d065e77c8cC2239327C5EDb3A432268e5831;
    address constant USDT = 0xFd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9;
    address constant ARB = 0x912CE59144191C1204E64559FE8253a0e49E6548;

    // Protocol addresses
    address constant AAVE_V3_POOL = 0x794a61358D6845594F94dc1DB02A252b5b4814aD;
    address constant RADIANT_LENDING_POOL = 0xF4B1486DD74D07706052A33d31d7c0AAFD0659E1;
    uint24 constant UNISWAP_WETH_USDC_FEE = 500;

    function setUp() public {
        deployer = address(this);
        liquidator = new FlashLiquidator();

        // Deploy mock tokens for unit tests
        mockUSDC = new MockERC20("Mock USDC", "USDC", 6);
        mockWETH = new MockERC20("Mock WETH", "WETH", 18);
    }

    // Allow this test contract to receive ETH (it's the owner of the liquidator)
    receive() external payable {}

    // =========================================================================
    //                        DEPLOYMENT TESTS
    // =========================================================================

    function test_deployment_setsOwner() public view {
        assertEq(liquidator.owner(), deployer);
    }

    function test_deployment_constantAddresses() public view {
        assertEq(liquidator.AAVE_V3_POOL(), AAVE_V3_POOL);
        assertEq(liquidator.RADIANT_LENDING_POOL(), RADIANT_LENDING_POOL);
    }

    // =========================================================================
    //                        OWNERSHIP TESTS
    // =========================================================================

    function test_transferOwnership_twoStep() public {
        address newOwner = makeAddr("newOwner");

        // Step 1: initiate transfer (owner stays the same)
        liquidator.transferOwnership(newOwner);
        assertEq(liquidator.owner(), deployer);
        assertEq(liquidator.pendingOwner(), newOwner);

        // Step 2: new owner accepts
        vm.prank(newOwner);
        liquidator.acceptOwnership();
        assertEq(liquidator.owner(), newOwner);
        assertEq(liquidator.pendingOwner(), address(0));
    }

    function test_acceptOwnership_revertsForNonPendingOwner() public {
        address newOwner = makeAddr("newOwner");
        liquidator.transferOwnership(newOwner);

        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.OnlyOwner.selector);
        liquidator.acceptOwnership();
    }

    function test_transferOwnership_revertsForNonOwner() public {
        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.OnlyOwner.selector);
        liquidator.transferOwnership(attacker);
    }

    function test_transferOwnership_revertsForZeroAddress() public {
        vm.expectRevert(FlashLiquidator.ZeroAddress.selector);
        liquidator.transferOwnership(address(0));
    }

    // =========================================================================
    //                    FLASH LOAN CALLBACK AUTH TESTS
    // =========================================================================

    function test_aaveCallback_revertsIfNotPool() public {
        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.OnlyPool.selector);
        liquidator.executeOperation(
            address(mockUSDC), // asset
            1000e6, // amount
            5e6, // premium
            address(liquidator), // initiator
            "" // params
        );
    }

    function test_aaveCallback_revertsIfWrongInitiator() public {
        vm.prank(AAVE_V3_POOL);
        vm.expectRevert(FlashLiquidator.OnlyPool.selector);
        liquidator.executeOperation(
            address(mockUSDC),
            1000e6,
            5e6,
            attacker, // wrong initiator
            ""
        );
    }

    function test_radiantCallback_revertsIfNotPool() public {
        address[] memory assets = new address[](1);
        assets[0] = address(mockUSDC);
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 1000e6;
        uint256[] memory premiums = new uint256[](1);
        premiums[0] = 5e6;

        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.OnlyPool.selector);
        liquidator.executeOperation(assets, amounts, premiums, address(liquidator), "");
    }

    function test_radiantCallback_revertsIfWrongInitiator() public {
        address[] memory assets = new address[](1);
        assets[0] = address(mockUSDC);
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = 1000e6;
        uint256[] memory premiums = new uint256[](1);
        premiums[0] = 5e6;

        vm.prank(RADIANT_LENDING_POOL);
        vm.expectRevert(FlashLiquidator.OnlyPool.selector);
        liquidator.executeOperation(assets, amounts, premiums, attacker, "");
    }

    // =========================================================================
    //                    ONLY OWNER ACCESS CONTROL TESTS
    // =========================================================================

    function test_liquidateWithAaveFlashLoan_onlyOwner() public {
        FlashLiquidator.LiquidationParams memory params;
        params.debtAsset = address(mockUSDC);
        params.collateralAsset = address(mockWETH);
        params.user = user;
        params.debtToCover = 1000e6;

        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.OnlyOwner.selector);
        liquidator.liquidateWithAaveFlashLoan(params);
    }

    function test_liquidateWithRadiantFlashLoan_onlyOwner() public {
        FlashLiquidator.LiquidationParams memory params;
        params.debtAsset = address(mockUSDC);
        params.collateralAsset = address(mockWETH);
        params.user = user;
        params.debtToCover = 1000e6;

        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.OnlyOwner.selector);
        liquidator.liquidateWithRadiantFlashLoan(params);
    }

    function test_liquidateSiloWithAaveFlashLoan_onlyOwner() public {
        FlashLiquidator.LiquidationParams memory params;
        params.protocol = FlashLiquidator.Protocol.Silo;
        params.debtAsset = address(mockUSDC);
        params.collateralAsset = address(mockWETH);
        params.user = user;
        params.debtToCover = 1000e6;
        params.siloAddress = makeAddr("silo");

        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.OnlyOwner.selector);
        liquidator.liquidateSiloWithAaveFlashLoan(params);
    }

    // =========================================================================
    //                    EMERGENCY WITHDRAW TESTS
    // =========================================================================

    function test_emergencyWithdraw_onlyOwner() public {
        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.OnlyOwner.selector);
        liquidator.emergencyWithdraw(address(mockUSDC), 1000e6);
    }

    function test_emergencyWithdraw_transfersTokens() public {
        uint256 amount = 1000e6;
        mockUSDC.mint(address(liquidator), amount);

        uint256 ownerBefore = mockUSDC.balanceOf(deployer);
        liquidator.emergencyWithdraw(address(mockUSDC), amount);
        uint256 ownerAfter = mockUSDC.balanceOf(deployer);

        assertEq(ownerAfter - ownerBefore, amount);
        assertEq(mockUSDC.balanceOf(address(liquidator)), 0);
    }

    function test_emergencyWithdraw_capsAtBalance() public {
        uint256 amount = 500e6;
        mockUSDC.mint(address(liquidator), amount);

        liquidator.emergencyWithdraw(address(mockUSDC), type(uint256).max);

        assertEq(mockUSDC.balanceOf(deployer), amount);
        assertEq(mockUSDC.balanceOf(address(liquidator)), 0);
    }

    function test_emergencyWithdrawETH_onlyOwner() public {
        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.OnlyOwner.selector);
        liquidator.emergencyWithdrawETH();
    }

    function test_emergencyWithdrawETH_transfersETH() public {
        uint256 amount = 1 ether;
        vm.deal(address(liquidator), amount);

        uint256 ownerBefore = deployer.balance;
        liquidator.emergencyWithdrawETH();
        uint256 ownerAfter = deployer.balance;

        assertEq(ownerAfter - ownerBefore, amount);
        assertEq(address(liquidator).balance, 0);
    }

    function test_receiveETH() public {
        vm.deal(address(this), 1 ether);
        (bool success,) = address(liquidator).call{value: 1 ether}("");
        assertTrue(success);
        assertEq(address(liquidator).balance, 1 ether);
    }

    // =========================================================================
    //                    PROTOCOL VALIDATION TESTS
    // =========================================================================

    function test_siloFlashLoan_revertsNotYetSupported() public {
        FlashLiquidator.LiquidationParams memory params;
        params.protocol = FlashLiquidator.Protocol.Silo;
        params.debtAsset = address(mockUSDC);
        params.collateralAsset = address(mockWETH);
        params.user = user;
        params.debtToCover = 1000e6;
        params.siloAddress = makeAddr("silo");

        vm.expectRevert("Silo not yet supported");
        liquidator.liquidateSiloWithAaveFlashLoan(params);
    }

    // =========================================================================
    //             FORK INTEGRATION TESTS (require Arbitrum RPC)
    // =========================================================================

    /// @notice End-to-end fork test for the Aave flash loan path.
    /// @dev Uses real Arbitrum token/router state and a mocked pool at the live pool address
    ///      so we can deterministically test flash loan -> liquidation -> swap -> repayment.
    function test_fork_aaveFlashLoan_executesSwapRepaysPoolAndPaysOwner() public {
        if (!_createArbitrumFork()) return;

        MockAaveV3Pool pool = _installMockAavePool();

        uint256 debtToCover = 1_000e6;
        uint256 premium = debtToCover * 9 / 10_000;
        uint256 collateralOut = 2 ether;
        address borrower = makeAddr("aaveBorrower");

        deal(USDC, AAVE_V3_POOL, 50_000e6);
        deal(WETH, AAVE_V3_POOL, collateralOut);

        pool.configure(WETH, USDC, borrower, debtToCover, collateralOut, 9);

        FlashLiquidator.LiquidationParams memory params = _baseParams(
            FlashLiquidator.Protocol.AaveV3, borrower, debtToCover, premium
        );

        uint256 ownerUsdcBefore = IERC20(USDC).balanceOf(deployer);
        uint256 poolUsdcBefore = IERC20(USDC).balanceOf(AAVE_V3_POOL);

        liquidator.liquidateWithAaveFlashLoan(params);

        uint256 ownerProfit = IERC20(USDC).balanceOf(deployer) - ownerUsdcBefore;
        uint256 poolUsdcAfter = IERC20(USDC).balanceOf(AAVE_V3_POOL);

        assertGe(ownerProfit, params.minProfit, "owner should receive realized profit");
        assertEq(
            poolUsdcAfter,
            poolUsdcBefore + debtToCover + premium,
            "pool should receive repaid debt plus flash loan premium"
        );
        assertEq(IERC20(USDC).balanceOf(address(liquidator)), 0, "contract should not retain debt tokens");
        assertEq(IERC20(WETH).balanceOf(address(liquidator)), 0, "contract should not retain collateral");
    }

    /// @notice End-to-end fork test for the Radiant flash loan path.
    function test_fork_radiantFlashLoan_executesSwapRepaysPoolAndPaysOwner() public {
        if (!_createArbitrumFork()) return;

        MockRadiantPool pool = _installMockRadiantPool();

        uint256 debtToCover = 1_000e6;
        uint256 premium = debtToCover * 9 / 10_000;
        uint256 collateralOut = 2 ether;
        address borrower = makeAddr("radiantBorrower");

        deal(USDC, RADIANT_LENDING_POOL, 50_000e6);
        deal(WETH, RADIANT_LENDING_POOL, collateralOut);

        pool.configure(WETH, USDC, borrower, debtToCover, collateralOut, 9);

        FlashLiquidator.LiquidationParams memory params = _baseParams(
            FlashLiquidator.Protocol.Radiant, borrower, debtToCover, premium
        );

        uint256 ownerUsdcBefore = IERC20(USDC).balanceOf(deployer);
        uint256 poolUsdcBefore = IERC20(USDC).balanceOf(RADIANT_LENDING_POOL);

        liquidator.liquidateWithRadiantFlashLoan(params);

        uint256 ownerProfit = IERC20(USDC).balanceOf(deployer) - ownerUsdcBefore;
        uint256 poolUsdcAfter = IERC20(USDC).balanceOf(RADIANT_LENDING_POOL);

        assertGe(ownerProfit, params.minProfit, "owner should receive realized profit");
        assertEq(
            poolUsdcAfter,
            poolUsdcBefore + debtToCover + premium,
            "pool should receive repaid debt plus flash loan premium"
        );
        assertEq(IERC20(USDC).balanceOf(address(liquidator)), 0, "contract should not retain debt tokens");
        assertEq(IERC20(WETH).balanceOf(address(liquidator)), 0, "contract should not retain collateral");
    }

    // =========================================================================
    //                    FUZZ TESTS
    // =========================================================================

    function testFuzz_emergencyWithdraw_neverExceedsBalance(uint256 amount) public {
        uint256 balance = 1000e6;
        mockUSDC.mint(address(liquidator), balance);

        uint256 ownerBefore = mockUSDC.balanceOf(deployer);
        liquidator.emergencyWithdraw(address(mockUSDC), amount);
        uint256 ownerAfter = mockUSDC.balanceOf(deployer);

        uint256 expectedWithdraw = amount > balance ? balance : amount;
        assertEq(ownerAfter - ownerBefore, expectedWithdraw);
    }

    function _createArbitrumFork() internal returns (bool) {
        string memory rpcUrl;

        try vm.envString("ARBITRUM_RPC_URL") returns (string memory url) {
            rpcUrl = url;
        } catch {
            console2.log("Skipping fork test: ARBITRUM_RPC_URL not configured");
            return false;
        }

        vm.createSelectFork(rpcUrl);
        liquidator = new FlashLiquidator();
        return true;
    }

    function _installMockAavePool() internal returns (MockAaveV3Pool pool) {
        MockAaveV3Pool implementation = new MockAaveV3Pool();
        vm.etch(AAVE_V3_POOL, address(implementation).code);
        pool = MockAaveV3Pool(AAVE_V3_POOL);
    }

    function _installMockRadiantPool() internal returns (MockRadiantPool pool) {
        MockRadiantPool implementation = new MockRadiantPool();
        vm.etch(RADIANT_LENDING_POOL, address(implementation).code);
        pool = MockRadiantPool(RADIANT_LENDING_POOL);
    }

    function _baseParams(
        FlashLiquidator.Protocol protocol,
        address borrower,
        uint256 debtToCover,
        uint256 premium
    ) internal pure returns (FlashLiquidator.LiquidationParams memory params) {
        params.protocol = protocol;
        params.collateralAsset = WETH;
        params.debtAsset = USDC;
        params.user = borrower;
        params.debtToCover = debtToCover;
        params.swapDex = SwapHelper.DEX.UniswapV3;
        params.swapFee = UNISWAP_WETH_USDC_FEE;
        params.minProfit = 100e6;
        params.swapPath = "";
        params.siloAddress = address(0);
        params.minAmountOut = debtToCover + premium;
    }
}

contract MockAaveV3Pool {
    using SafeERC20 for IERC20;

    address public collateralAsset;
    address public debtAsset;
    address public expectedUser;
    uint256 public expectedDebtToCover;
    uint256 public collateralOut;
    uint128 public premiumBps;

    function configure(
        address collateralAsset_,
        address debtAsset_,
        address expectedUser_,
        uint256 expectedDebtToCover_,
        uint256 collateralOut_,
        uint128 premiumBps_
    ) external {
        collateralAsset = collateralAsset_;
        debtAsset = debtAsset_;
        expectedUser = expectedUser_;
        expectedDebtToCover = expectedDebtToCover_;
        collateralOut = collateralOut_;
        premiumBps = premiumBps_;
    }

    function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128) {
        return premiumBps;
    }

    function flashLoanSimple(
        address receiverAddress,
        address asset,
        uint256 amount,
        bytes calldata params,
        uint16
    ) external {
        uint256 premium = amount * premiumBps / 10_000;
        IERC20(asset).safeTransfer(receiverAddress, amount);

        bool ok = IFlashLoanSimpleReceiver(receiverAddress).executeOperation(
            asset, amount, premium, receiverAddress, params
        );
        require(ok, "callback failed");

        IERC20(asset).safeTransferFrom(receiverAddress, address(this), amount + premium);
    }

    function liquidationCall(address collateralAsset_, address debtAsset_, address user, uint256 debtToCover, bool)
        external
    {
        require(collateralAsset_ == collateralAsset, "bad collateral");
        require(debtAsset_ == debtAsset, "bad debt asset");
        require(user == expectedUser, "bad user");
        require(debtToCover == expectedDebtToCover, "bad debt amount");

        IERC20(debtAsset_).safeTransferFrom(msg.sender, address(this), debtToCover);
        IERC20(collateralAsset_).safeTransfer(msg.sender, collateralOut);
    }
}

contract MockRadiantPool {
    using SafeERC20 for IERC20;

    address public collateralAsset;
    address public debtAsset;
    address public expectedUser;
    uint256 public expectedDebtToCover;
    uint256 public collateralOut;
    uint256 public premiumBps;

    function configure(
        address collateralAsset_,
        address debtAsset_,
        address expectedUser_,
        uint256 expectedDebtToCover_,
        uint256 collateralOut_,
        uint256 premiumBps_
    ) external {
        collateralAsset = collateralAsset_;
        debtAsset = debtAsset_;
        expectedUser = expectedUser_;
        expectedDebtToCover = expectedDebtToCover_;
        collateralOut = collateralOut_;
        premiumBps = premiumBps_;
    }

    function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint256) {
        return premiumBps;
    }

    function flashLoan(
        address receiverAddress,
        address[] calldata assets,
        uint256[] calldata amounts,
        uint256[] calldata,
        address,
        bytes calldata params,
        uint16
    ) external {
        _flashLoan(receiverAddress, assets[0], amounts[0], params);
    }

    function _flashLoan(address receiverAddress, address asset, uint256 amount, bytes calldata params) internal {
        uint256 premium = amount * premiumBps / 10_000;
        address[] memory assets = new address[](1);
        assets[0] = asset;
        uint256[] memory amounts = new uint256[](1);
        amounts[0] = amount;
        uint256[] memory premiums = new uint256[](1);
        premiums[0] = premium;

        IERC20(asset).safeTransfer(receiverAddress, amount);

        bool ok = IFlashLoanReceiver(receiverAddress).executeOperation(
            assets, amounts, premiums, receiverAddress, params
        );
        require(ok, "callback failed");

        IERC20(asset).safeTransferFrom(receiverAddress, address(this), amount + premium);
    }

    function liquidationCall(address collateralAsset_, address debtAsset_, address user, uint256 debtToCover, bool)
        external
    {
        require(collateralAsset_ == collateralAsset, "bad collateral");
        require(debtAsset_ == debtAsset, "bad debt asset");
        require(user == expectedUser, "bad user");
        require(debtToCover == expectedDebtToCover, "bad debt amount");

        IERC20(debtAsset_).safeTransferFrom(msg.sender, address(this), debtToCover);
        IERC20(collateralAsset_).safeTransfer(msg.sender, collateralOut);
    }
}

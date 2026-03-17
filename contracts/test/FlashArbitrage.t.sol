// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test, console2} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {FlashArbitrage} from "../src/FlashArbitrage.sol";
import {IUniswapV3SwapCallback} from "../src/interfaces/IUniswapV3Pool.sol";

// =========================================================================
//                          MOCK CONTRACTS
// =========================================================================

contract MockERC20Arb is ERC20 {
    uint8 private _decimals;

    constructor(string memory name_, string memory symbol_, uint8 dec_) ERC20(name_, symbol_) {
        _decimals = dec_;
    }

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    function decimals() public view override returns (uint8) {
        return _decimals;
    }
}

/// @dev Mock UniV3 pool for testing flash swap arbitrage.
///      Returns pre-configured swap deltas and verifies callback payments.
contract MockUniV3Pool {
    using SafeERC20 for IERC20;

    address public token0;
    address public token1;
    uint24 public fee;

    int256 private _amount0Delta;
    int256 private _amount1Delta;

    constructor(address token0_, address token1_, uint24 fee_) {
        token0 = token0_;
        token1 = token1_;
        fee = fee_;
    }

    function setSwapResult(int256 amount0Delta_, int256 amount1Delta_) external {
        _amount0Delta = amount0Delta_;
        _amount1Delta = amount1Delta_;
    }

    function slot0() external pure returns (uint160, int24, uint16, uint16, uint16, uint8, bool) {
        return (0, 0, 0, 0, 0, 0, true);
    }

    function liquidity() external pure returns (uint128) {
        return 1e18;
    }

    function swap(
        address recipient,
        bool, /* zeroForOne */
        int256, /* amountSpecified */
        uint160, /* sqrtPriceLimitX96 */
        bytes calldata data
    )
        external
        returns (int256, int256)
    {
        // Transfer output tokens to recipient (negative deltas = pool sends)
        if (_amount0Delta < 0) {
            IERC20(token0).safeTransfer(recipient, uint256(-_amount0Delta));
        }
        if (_amount1Delta < 0) {
            IERC20(token1).safeTransfer(recipient, uint256(-_amount1Delta));
        }

        // Record balances before callback to verify payment
        uint256 balance0Before = IERC20(token0).balanceOf(address(this));
        uint256 balance1Before = IERC20(token1).balanceOf(address(this));

        // Call the swap callback
        IUniswapV3SwapCallback(recipient).uniswapV3SwapCallback(_amount0Delta, _amount1Delta, data);

        // Verify the callback paid us what we're owed (positive deltas)
        if (_amount0Delta > 0) {
            require(
                IERC20(token0).balanceOf(address(this)) >= balance0Before + uint256(_amount0Delta),
                "MockPool: insufficient token0 payment"
            );
        }
        if (_amount1Delta > 0) {
            require(
                IERC20(token1).balanceOf(address(this)) >= balance1Before + uint256(_amount1Delta),
                "MockPool: insufficient token1 payment"
            );
        }

        return (_amount0Delta, _amount1Delta);
    }
}

// =========================================================================
//                          TEST CONTRACT
// =========================================================================

/// @title FlashArbitrageTest
/// @notice Unit tests for the FlashArbitrage contract
contract FlashArbitrageTest is Test {
    FlashArbitrage public flashArb;

    MockERC20Arb public tokenA;
    MockERC20Arb public tokenB;
    address public t0; // lower address (token0)
    address public t1; // higher address (token1)

    MockUniV3Pool public poolA;
    MockUniV3Pool public poolB;

    address public deployer;
    address public attacker = makeAddr("attacker");

    function setUp() public {
        deployer = address(this);
        flashArb = new FlashArbitrage();

        // Deploy mock tokens
        tokenA = new MockERC20Arb("Token A", "A", 18);
        tokenB = new MockERC20Arb("Token B", "B", 18);

        // Determine token0/token1 ordering (UniV3 convention: token0 < token1)
        if (address(tokenA) < address(tokenB)) {
            t0 = address(tokenA);
            t1 = address(tokenB);
        } else {
            t0 = address(tokenB);
            t1 = address(tokenA);
        }

        // Deploy mock pools with same token pair
        poolA = new MockUniV3Pool(t0, t1, 500); // 0.05% fee
        poolB = new MockUniV3Pool(t0, t1, 3000); // 0.3% fee
    }

    // Allow this test contract to receive ETH
    receive() external payable {}

    // =========================================================================
    //                        DEPLOYMENT TESTS
    // =========================================================================

    function test_deployment_setsOwner() public view {
        assertEq(flashArb.owner(), deployer);
    }

    // =========================================================================
    //                        OWNERSHIP / ACCESS CONTROL
    // =========================================================================

    function test_executeArbitrage_onlyOwner() public {
        FlashArbitrage.ArbParams memory params;
        params.poolA = address(poolA);
        params.poolB = address(poolB);
        params.amountIn = 1e18;

        vm.prank(attacker);
        vm.expectRevert(FlashArbitrage.OnlyOwner.selector);
        flashArb.executeArbitrage(params);
    }

    function test_executeArbitrage_revertsForZeroAmount() public {
        FlashArbitrage.ArbParams memory params;
        params.poolA = address(poolA);
        params.poolB = address(poolB);
        params.amountIn = 0;

        vm.expectRevert(FlashArbitrage.InvalidAmount.selector);
        flashArb.executeArbitrage(params);
    }

    function test_executeArbitrage_revertsForSamePool() public {
        FlashArbitrage.ArbParams memory params;
        params.poolA = address(poolA);
        params.poolB = address(poolA);
        params.amountIn = 1e18;

        vm.expectRevert(FlashArbitrage.InvalidPoolPair.selector);
        flashArb.executeArbitrage(params);
    }

    function test_executeArbitrage_revertsForMismatchedPoolTokens() public {
        MockERC20Arb tokenC = new MockERC20Arb("Token C", "C", 18);
        MockUniV3Pool badPool = new MockUniV3Pool(t0, address(tokenC), 500);

        FlashArbitrage.ArbParams memory params;
        params.poolA = address(poolA);
        params.poolB = address(badPool);
        params.amountIn = 1e18;

        vm.expectRevert(FlashArbitrage.InvalidPoolPair.selector);
        flashArb.executeArbitrage(params);
    }

    function test_transferOwnership_twoStep() public {
        address newOwner = makeAddr("newOwner");

        flashArb.transferOwnership(newOwner);
        assertEq(flashArb.pendingOwner(), newOwner);
        assertEq(flashArb.owner(), deployer);

        vm.prank(newOwner);
        flashArb.acceptOwnership();

        assertEq(flashArb.owner(), newOwner);
        assertEq(flashArb.pendingOwner(), address(0));
    }

    function test_transferOwnership_revertsForZeroAddress() public {
        vm.expectRevert(FlashArbitrage.ZeroAddress.selector);
        flashArb.transferOwnership(address(0));
    }

    function test_acceptOwnership_revertsForNonPendingOwner() public {
        vm.prank(attacker);
        vm.expectRevert(FlashArbitrage.OnlyOwner.selector);
        flashArb.acceptOwnership();
    }

    function test_emergencyWithdraw_onlyOwner() public {
        vm.prank(attacker);
        vm.expectRevert(FlashArbitrage.OnlyOwner.selector);
        flashArb.emergencyWithdraw(t0, 1e18);
    }

    function test_emergencyWithdrawETH_onlyOwner() public {
        vm.prank(attacker);
        vm.expectRevert(FlashArbitrage.OnlyOwner.selector);
        flashArb.emergencyWithdrawETH();
    }

    // =========================================================================
    //                    CALLBACK AUTH TESTS
    // =========================================================================

    function test_callback_revertsWhenNotExecuting() public {
        vm.prank(address(poolA));
        vm.expectRevert(FlashArbitrage.InvalidCallback.selector);
        flashArb.uniswapV3SwapCallback(0, 0, "");
    }

    // =========================================================================
    //                    EMERGENCY WITHDRAW TESTS
    // =========================================================================

    function test_emergencyWithdraw_transfersTokens() public {
        uint256 amount = 1000e18;
        MockERC20Arb(t0).mint(address(flashArb), amount);

        uint256 ownerBefore = IERC20(t0).balanceOf(deployer);
        flashArb.emergencyWithdraw(t0, amount);
        uint256 ownerAfter = IERC20(t0).balanceOf(deployer);

        assertEq(ownerAfter - ownerBefore, amount);
        assertEq(IERC20(t0).balanceOf(address(flashArb)), 0);
    }

    function test_emergencyWithdraw_capsAtBalance() public {
        uint256 amount = 500e18;
        MockERC20Arb(t0).mint(address(flashArb), amount);

        flashArb.emergencyWithdraw(t0, type(uint256).max);
        assertEq(IERC20(t0).balanceOf(deployer), amount);
    }

    function test_emergencyWithdrawETH() public {
        vm.deal(address(flashArb), 1 ether);

        uint256 before = deployer.balance;
        flashArb.emergencyWithdrawETH();

        assertEq(deployer.balance - before, 1 ether);
        assertEq(address(flashArb).balance, 0);
    }

    // =========================================================================
    //                    FULL FLASH SWAP ARBITRAGE TEST
    // =========================================================================

    /// @notice Test the complete flash swap arbitrage flow with mock pools.
    ///
    /// Flow:
    ///   1. poolA.swap(zeroForOne=true) → poolA sends 2e18 token1, expects 1e18 token0
    ///   2. In poolA callback: swap 2e18 token1 on poolB (zeroForOne=false)
    ///      → poolB sends 1.5e18 token0, expects 2e18 token1
    ///   3. In poolB callback: pay poolB 2e18 token1 (from poolA)
    ///   4. Back in poolA callback: pay poolA 1e18 token0, profit = 0.5e18 token0
    function test_fullArbitrageFlow() public {
        // Configure pool A: zeroForOne=true swap
        // amount0Delta = 1e18 (owe 1 token0), amount1Delta = -2e18 (receive 2 token1)
        poolA.setSwapResult(int256(1e18), -int256(2e18));

        // Configure pool B: zeroForOne=false swap (reverse direction)
        // amount0Delta = -1.5e18 (receive 1.5 token0), amount1Delta = 2e18 (owe 2 token1)
        poolB.setSwapResult(-int256(1.5e18), int256(2e18));

        // Fund pools with output tokens
        MockERC20Arb(t1).mint(address(poolA), 2e18); // poolA sends token1
        MockERC20Arb(t0).mint(address(poolB), 1.5e18); // poolB sends token0

        // Execute arbitrage
        FlashArbitrage.ArbParams memory params = FlashArbitrage.ArbParams({
            poolA: address(poolA), poolB: address(poolB), zeroForOne: true, amountIn: int256(1e18), minProfit: 0
        });

        uint256 ownerToken0Before = IERC20(t0).balanceOf(deployer);
        flashArb.executeArbitrage(params);
        uint256 ownerToken0After = IERC20(t0).balanceOf(deployer);

        // Profit = 1.5e18 (from poolB) - 1e18 (paid to poolA) = 0.5e18 token0
        uint256 profit = ownerToken0After - ownerToken0Before;
        assertEq(profit, 0.5e18, "Owner should receive 0.5e18 profit in token0");

        // Contract should not retain any tokens
        assertEq(IERC20(t0).balanceOf(address(flashArb)), 0, "No residual token0");
        assertEq(IERC20(t1).balanceOf(address(flashArb)), 0, "No residual token1");
    }

    /// @notice Test arbitrage in the opposite direction (zeroForOne=false on poolA)
    function test_fullArbitrageFlow_reverseDirection() public {
        // Configure pool A: zeroForOne=false swap
        // amount0Delta = -2e18 (receive 2 token0), amount1Delta = 1e18 (owe 1 token1)
        poolA.setSwapResult(-int256(2e18), int256(1e18));

        // Configure pool B: zeroForOne=true swap (reverse)
        // amount0Delta = 2e18 (owe 2 token0), amount1Delta = -1.5e18 (receive 1.5 token1)
        poolB.setSwapResult(int256(2e18), -int256(1.5e18));

        // Fund pools with output tokens
        MockERC20Arb(t0).mint(address(poolA), 2e18); // poolA sends token0
        MockERC20Arb(t1).mint(address(poolB), 1.5e18); // poolB sends token1

        FlashArbitrage.ArbParams memory params = FlashArbitrage.ArbParams({
            poolA: address(poolA), poolB: address(poolB), zeroForOne: false, amountIn: int256(1e18), minProfit: 0
        });

        uint256 ownerToken1Before = IERC20(t1).balanceOf(deployer);
        flashArb.executeArbitrage(params);
        uint256 ownerToken1After = IERC20(t1).balanceOf(deployer);

        // Profit = 1.5e18 (from poolB) - 1e18 (paid to poolA) = 0.5e18 token1
        uint256 profit = ownerToken1After - ownerToken1Before;
        assertEq(profit, 0.5e18, "Owner should receive 0.5e18 profit in token1");
    }

    /// @notice Test that minProfit enforcement works
    function test_minProfit_reverts() public {
        poolA.setSwapResult(int256(1e18), -int256(2e18));
        poolB.setSwapResult(-int256(1.5e18), int256(2e18));
        MockERC20Arb(t1).mint(address(poolA), 2e18);
        MockERC20Arb(t0).mint(address(poolB), 1.5e18);

        FlashArbitrage.ArbParams memory params = FlashArbitrage.ArbParams({
            poolA: address(poolA),
            poolB: address(poolB),
            zeroForOne: true,
            amountIn: int256(1e18),
            minProfit: 1e18 // require 1e18 profit, but only 0.5e18 available
        });

        vm.expectRevert(abi.encodeWithSelector(FlashArbitrage.InsufficientProfit.selector, 0.5e18, 1e18));
        flashArb.executeArbitrage(params);
    }

    /// @notice Test that an unprofitable arb reverts (poolB gives less than poolA needs)
    function test_unprofitableArb_reverts() public {
        // Pool A: owe 1e18 token0, receive 2e18 token1
        poolA.setSwapResult(int256(1e18), -int256(2e18));
        // Pool B: receive only 0.8e18 token0 (less than 1e18 owed to poolA)
        poolB.setSwapResult(-int256(0.8e18), int256(2e18));
        MockERC20Arb(t1).mint(address(poolA), 2e18);
        MockERC20Arb(t0).mint(address(poolB), 0.8e18);

        FlashArbitrage.ArbParams memory params = FlashArbitrage.ArbParams({
            poolA: address(poolA), poolB: address(poolB), zeroForOne: true, amountIn: int256(1e18), minProfit: 0
        });

        // Should revert because we can't pay poolA (0.8e18 < 1e18)
        vm.expectRevert();
        flashArb.executeArbitrage(params);
    }

    // =========================================================================
    //                    REENTRANCY TEST
    // =========================================================================

    function test_reentrancy_reverts() public {
        // `pendingOwner` and `_executing` are packed into slot 1. Set `_executing = true`.
        vm.store(address(flashArb), bytes32(uint256(1)), bytes32(uint256(1) << 160));

        FlashArbitrage.ArbParams memory params =
            FlashArbitrage.ArbParams({poolA: address(poolA), poolB: address(poolB), zeroForOne: true, amountIn: 1e18, minProfit: 0});

        vm.expectRevert(FlashArbitrage.Reentrancy.selector);
        flashArb.executeArbitrage(params);
    }

    // =========================================================================
    //                    FUZZ TESTS
    // =========================================================================

    function testFuzz_emergencyWithdraw_neverExceedsBalance(uint256 amount) public {
        uint256 balance = 1000e18;
        MockERC20Arb(t0).mint(address(flashArb), balance);

        uint256 ownerBefore = IERC20(t0).balanceOf(deployer);
        flashArb.emergencyWithdraw(t0, amount);
        uint256 ownerAfter = IERC20(t0).balanceOf(deployer);

        uint256 expectedWithdraw = amount > balance ? balance : amount;
        assertEq(ownerAfter - ownerBefore, expectedWithdraw);
    }

    // =========================================================================
    //                    MULTI-HOP TESTS
    // =========================================================================

    /// @notice 3-pool triangular arb: t0 → t1 → tC → t0
    ///   Pool 0 (t0/t1): give 1 t0, get 2 t1
    ///   Pool 1 (t1/tC): give 2 t1, get 3 tC
    ///   Pool 2 (tC/t0): give 3 tC, get 1.5 t0
    ///   Profit: 1.5 - 1 = 0.5 t0
    function test_multiHop_3pools() public {
        MockERC20Arb tokenC = new MockERC20Arb("Token C", "C", 18);

        // Create ordered pools (token0 < token1 per UniV3)
        // Pool 0: t0/t1 (already exists as poolA)
        // Pool 1: need t1/tC or tC/t1 depending on address order
        // Pool 2: need t0/tC or tC/t0 depending on address order

        (address tc_t0, address tc_t1) = address(tokenC) < t0 ? (address(tokenC), t0) : (t0, address(tokenC));

        (address tc2_t0, address tc2_t1) = t1 < address(tokenC) ? (t1, address(tokenC)) : (address(tokenC), t1);

        MockUniV3Pool pool1 = new MockUniV3Pool(tc2_t0, tc2_t1, 500);
        MockUniV3Pool pool2 = new MockUniV3Pool(tc_t0, tc_t1, 3000);

        // Pool 0 (t0/t1): zeroForOne=true → owe t0, get t1
        poolA.setSwapResult(int256(1e18), -int256(2e18));
        MockERC20Arb(t1).mint(address(poolA), 2e18);

        // Pool 1: we have t1, swap for tC
        // Need to figure out direction based on token ordering
        if (t1 < address(tokenC)) {
            // pool1 is (t1, tC), zeroForOne=true means give t1 get tC
            pool1.setSwapResult(int256(2e18), -int256(3e18));
            tokenC.mint(address(pool1), 3e18);
        } else {
            // pool1 is (tC, t1), zeroForOne=false means give t1 get tC
            pool1.setSwapResult(-int256(3e18), int256(2e18));
            tokenC.mint(address(pool1), 3e18);
        }

        // Pool 2: we have tC, swap for t0
        if (address(tokenC) < t0) {
            // pool2 is (tC, t0), zeroForOne=true means give tC get t0
            // But we want to give tC and get t0, so zeroForOne=true
            pool2.setSwapResult(int256(3e18), -int256(1.5e18));
            MockERC20Arb(t0).mint(address(pool2), 1.5e18);
        } else {
            // pool2 is (t0, tC), zeroForOne=false means give tC get t0
            pool2.setSwapResult(-int256(1.5e18), int256(3e18));
            MockERC20Arb(t0).mint(address(pool2), 1.5e18);
        }

        // Build zeroForOne array
        bool[] memory zfo = new bool[](3);
        zfo[0] = true; // pool0: give t0, get t1
        zfo[1] = t1 < address(tokenC); // pool1: depends on ordering
        zfo[2] = address(tokenC) < t0
            ? true  // pool2 is (tC, t0): zeroForOne=true gives tC gets t0
            : false; // pool2 is (t0, tC): zeroForOne=false gives tC gets t0

        address[] memory pools = new address[](3);
        pools[0] = address(poolA);
        pools[1] = address(pool1);
        pools[2] = address(pool2);

        FlashArbitrage.MultiHopParams memory params =
            FlashArbitrage.MultiHopParams({pools: pools, zeroForOne: zfo, amountIn: int256(1e18), minProfit: 0});

        uint256 ownerBefore = IERC20(t0).balanceOf(deployer);
        flashArb.executeMultiHop(params);
        uint256 ownerAfter = IERC20(t0).balanceOf(deployer);

        assertEq(ownerAfter - ownerBefore, 0.5e18, "3-hop profit should be 0.5 t0");
        assertEq(IERC20(t0).balanceOf(address(flashArb)), 0, "No residual t0");
        assertEq(IERC20(t1).balanceOf(address(flashArb)), 0, "No residual t1");
        assertEq(tokenC.balanceOf(address(flashArb)), 0, "No residual tC");
    }

    /// @notice Multi-hop also works with 2 pools (same as legacy)
    function test_multiHop_2pools_sameAsLegacy() public {
        poolA.setSwapResult(int256(1e18), -int256(2e18));
        poolB.setSwapResult(-int256(1.5e18), int256(2e18));
        MockERC20Arb(t1).mint(address(poolA), 2e18);
        MockERC20Arb(t0).mint(address(poolB), 1.5e18);

        address[] memory pools = new address[](2);
        pools[0] = address(poolA);
        pools[1] = address(poolB);

        bool[] memory zfo = new bool[](2);
        zfo[0] = true; // pool0: give t0, get t1
        zfo[1] = false; // pool1: give t1, get t0

        FlashArbitrage.MultiHopParams memory params =
            FlashArbitrage.MultiHopParams({pools: pools, zeroForOne: zfo, amountIn: int256(1e18), minProfit: 0});

        uint256 ownerBefore = IERC20(t0).balanceOf(deployer);
        flashArb.executeMultiHop(params);
        uint256 profit = IERC20(t0).balanceOf(deployer) - ownerBefore;

        assertEq(profit, 0.5e18, "2-hop via multiHop should give same profit as legacy");
    }

    /// @notice Multi-hop with minProfit enforcement
    function test_multiHop_minProfit_reverts() public {
        poolA.setSwapResult(int256(1e18), -int256(2e18));
        poolB.setSwapResult(-int256(1.5e18), int256(2e18));
        MockERC20Arb(t1).mint(address(poolA), 2e18);
        MockERC20Arb(t0).mint(address(poolB), 1.5e18);

        address[] memory pools = new address[](2);
        pools[0] = address(poolA);
        pools[1] = address(poolB);

        bool[] memory zfo = new bool[](2);
        zfo[0] = true;
        zfo[1] = false;

        FlashArbitrage.MultiHopParams memory params = FlashArbitrage.MultiHopParams({
            pools: pools,
            zeroForOne: zfo,
            amountIn: int256(1e18),
            minProfit: 1e18 // want 1e18 but only 0.5e18 available
        });

        vm.expectRevert(abi.encodeWithSelector(FlashArbitrage.InsufficientProfit.selector, 0.5e18, 1e18));
        flashArb.executeMultiHop(params);
    }

    function test_multiHop_profitExcludesPreExistingDust() public {
        poolA.setSwapResult(int256(1e18), -int256(2e18));
        poolB.setSwapResult(-int256(1.5e18), int256(2e18));
        MockERC20Arb(t1).mint(address(poolA), 2e18);
        MockERC20Arb(t0).mint(address(poolB), 1.5e18);

        uint256 dust = 0.25e18;
        MockERC20Arb(t0).mint(address(flashArb), dust);

        address[] memory pools = new address[](2);
        pools[0] = address(poolA);
        pools[1] = address(poolB);

        bool[] memory zfo = new bool[](2);
        zfo[0] = true;
        zfo[1] = false;

        FlashArbitrage.MultiHopParams memory params =
            FlashArbitrage.MultiHopParams({pools: pools, zeroForOne: zfo, amountIn: int256(1e18), minProfit: 0});

        uint256 ownerBefore = IERC20(t0).balanceOf(deployer);
        flashArb.executeMultiHop(params);
        uint256 ownerAfter = IERC20(t0).balanceOf(deployer);

        assertEq(ownerAfter - ownerBefore, dust + 0.5e18, "Dust should be swept before execution and only new profit added");
        assertEq(IERC20(t0).balanceOf(address(flashArb)), 0, "Contract should not retain old dust after execution");
    }

    /// @notice Multi-hop rejects invalid route length
    function test_multiHop_invalidRoute() public {
        address[] memory pools = new address[](1);
        pools[0] = address(poolA);
        bool[] memory zfo = new bool[](1);
        zfo[0] = true;

        FlashArbitrage.MultiHopParams memory params =
            FlashArbitrage.MultiHopParams({pools: pools, zeroForOne: zfo, amountIn: 1e18, minProfit: 0});

        vm.expectRevert(FlashArbitrage.InvalidRoute.selector);
        flashArb.executeMultiHop(params);
    }

    /// @notice Multi-hop rejects mismatched arrays
    function test_multiHop_mismatchedArrays() public {
        address[] memory pools = new address[](2);
        pools[0] = address(poolA);
        pools[1] = address(poolB);
        bool[] memory zfo = new bool[](3); // wrong length

        FlashArbitrage.MultiHopParams memory params =
            FlashArbitrage.MultiHopParams({pools: pools, zeroForOne: zfo, amountIn: 1e18, minProfit: 0});

        vm.expectRevert(FlashArbitrage.InvalidRoute.selector);
        flashArb.executeMultiHop(params);
    }

    /// @notice Multi-hop onlyOwner
    function test_multiHop_onlyOwner() public {
        address[] memory pools = new address[](2);
        pools[0] = address(poolA);
        pools[1] = address(poolB);
        bool[] memory zfo = new bool[](2);

        FlashArbitrage.MultiHopParams memory params =
            FlashArbitrage.MultiHopParams({pools: pools, zeroForOne: zfo, amountIn: 1e18, minProfit: 0});

        vm.prank(attacker);
        vm.expectRevert(FlashArbitrage.OnlyOwner.selector);
        flashArb.executeMultiHop(params);
    }
}

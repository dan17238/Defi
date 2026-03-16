// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {IUniswapV3Pool, IUniswapV3SwapCallback} from "./interfaces/IUniswapV3Pool.sol";

/// @title FlashArbitrage
/// @notice Executes atomic UniV3 flash swap arbitrage between two pools with the same token pair.
/// @dev Uses nested callbacks: poolA.swap() → callback → poolB.swap() → callback.
///      The contract distinguishes callbacks via stored _arbPoolA address.
contract FlashArbitrage is IUniswapV3SwapCallback {
    using SafeERC20 for IERC20;

    // =========================================================================
    //                              CONSTANTS
    // =========================================================================

    /// @dev UniV3 MIN_SQRT_RATIO + 1 (used as "no limit" for zeroForOne=true swaps)
    uint160 private constant MIN_SQRT_RATIO_PLUS_ONE = 4295128740;

    /// @dev UniV3 MAX_SQRT_RATIO - 1 (used as "no limit" for zeroForOne=false swaps)
    uint160 private constant MAX_SQRT_RATIO_MINUS_ONE =
        1461446703485210103287273052203988822378723970341;

    // =========================================================================
    //                              STRUCTS
    // =========================================================================

    /// @notice Parameters for an arbitrage operation
    struct ArbParams {
        address poolA; // First pool (flash swap source)
        address poolB; // Second pool (counter-trade)
        bool zeroForOne; // Swap direction on poolA (reversed on poolB)
        int256 amountIn; // Amount to swap on poolA (positive = exact input)
        uint256 minProfit; // Minimum profit required (in profit token units)
    }

    // =========================================================================
    //                              STATE
    // =========================================================================

    /// @notice Owner of the contract (receives profits, can withdraw)
    address public owner;

    /// @dev Reentrancy guard
    bool private _executing;

    /// @dev Pool A address for the current arbitrage (distinguishes callbacks)
    address private _arbPoolA;

    /// @dev Pool B address for the current arbitrage (validates second callback)
    address private _arbPoolB;

    // =========================================================================
    //                              EVENTS
    // =========================================================================

    event ArbitrageExecuted(
        address indexed poolA,
        address indexed poolB,
        address tokenProfit,
        uint256 profit
    );

    event EmergencyWithdraw(address indexed token, uint256 amount);

    // =========================================================================
    //                              ERRORS
    // =========================================================================

    error OnlyOwner();
    error Reentrancy();
    error InvalidCallback();
    error InsufficientProfit(uint256 actual, uint256 required);

    // =========================================================================
    //                              MODIFIERS
    // =========================================================================

    modifier onlyOwner() {
        if (msg.sender != owner) revert OnlyOwner();
        _;
    }

    // =========================================================================
    //                            CONSTRUCTOR
    // =========================================================================

    constructor() {
        owner = msg.sender;
    }

    // =========================================================================
    //                         EXTERNAL FUNCTIONS
    // =========================================================================

    /// @notice Execute an atomic arbitrage between two UniV3 pools
    /// @param params The arbitrage parameters
    function executeArbitrage(ArbParams calldata params) external onlyOwner {
        if (_executing) revert Reentrancy();
        _executing = true;
        _arbPoolA = params.poolA;
        _arbPoolB = params.poolB;

        uint160 sqrtPriceLimit = params.zeroForOne
            ? MIN_SQRT_RATIO_PLUS_ONE
            : MAX_SQRT_RATIO_MINUS_ONE;

        // Initiate flash swap on poolA. The pool transfers output tokens first,
        // then calls uniswapV3SwapCallback where we execute the counter-trade.
        IUniswapV3Pool(params.poolA).swap(
            address(this),
            params.zeroForOne,
            params.amountIn,
            sqrtPriceLimit,
            abi.encode(params)
        );

        _arbPoolA = address(0);
        _arbPoolB = address(0);
        _executing = false;
    }

    /// @notice UniV3 swap callback — handles both poolA (first layer) and poolB (second layer)
    /// @dev msg.sender == _arbPoolA: first callback from poolA, execute counter-trade on poolB
    ///      msg.sender == _arbPoolB: second callback from poolB, pay poolB its owed tokens
    function uniswapV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external override {
        if (!_executing) revert InvalidCallback();

        if (msg.sender == _arbPoolA) {
            ArbParams memory params = abi.decode(data, (ArbParams));
            _handlePoolACallback(amount0Delta, amount1Delta, params);
        } else if (msg.sender == _arbPoolB) {
            _handlePoolBCallback(amount0Delta, amount1Delta);
        } else {
            revert InvalidCallback();
        }
    }

    // =========================================================================
    //                         ADMIN FUNCTIONS
    // =========================================================================

    /// @notice Emergency withdraw any ERC-20 token stuck in the contract
    function emergencyWithdraw(address token, uint256 amount) external onlyOwner {
        uint256 balance = IERC20(token).balanceOf(address(this));
        uint256 withdrawAmount = amount > balance ? balance : amount;
        if (withdrawAmount > 0) {
            IERC20(token).safeTransfer(owner, withdrawAmount);
            emit EmergencyWithdraw(token, withdrawAmount);
        }
    }

    /// @notice Emergency withdraw native ETH stuck in the contract
    function emergencyWithdrawETH() external onlyOwner {
        uint256 balance = address(this).balance;
        if (balance > 0) {
            (bool success,) = owner.call{value: balance}("");
            require(success, "ETH transfer failed");
            emit EmergencyWithdraw(address(0), balance);
        }
    }

    /// @notice Allow contract to receive ETH
    receive() external payable {}

    // =========================================================================
    //                        INTERNAL FUNCTIONS
    // =========================================================================

    /// @dev First-layer callback: called by poolA after it sends us output tokens.
    ///      We counter-trade on poolB and pay back poolA, keeping the profit.
    function _handlePoolACallback(
        int256 amount0Delta,
        int256 amount1Delta,
        ArbParams memory params
    ) internal {
        address token0 = IUniswapV3Pool(params.poolA).token0();
        address token1 = IUniswapV3Pool(params.poolA).token1();

        // Positive delta = we owe this token to poolA
        // Negative delta = poolA sent us this token
        address profitToken;
        uint256 amountOwed;
        uint256 amountReceived;

        if (amount0Delta > 0) {
            // Owe token0 to poolA, received token1
            profitToken = token0;
            amountOwed = uint256(amount0Delta);
            amountReceived = uint256(-amount1Delta);
        } else {
            // Owe token1 to poolA, received token0
            profitToken = token1;
            amountOwed = uint256(amount1Delta);
            amountReceived = uint256(-amount0Delta);
        }

        // Record profit token balance before poolB swap
        uint256 balanceBefore = IERC20(profitToken).balanceOf(address(this));

        // Counter-trade on poolB (reverse direction)
        bool zeroForOneB = !params.zeroForOne;
        uint160 sqrtPriceLimitB = zeroForOneB
            ? MIN_SQRT_RATIO_PLUS_ONE
            : MAX_SQRT_RATIO_MINUS_ONE;

        // Use all received tokens as exact input on poolB
        IUniswapV3Pool(params.poolB).swap(
            address(this),
            zeroForOneB,
            int256(amountReceived),
            sqrtPriceLimitB,
            "" // empty data — poolB callback reads from storage
        );

        // After poolB swap, we have profitToken from poolB
        uint256 balanceAfter = IERC20(profitToken).balanceOf(address(this));
        uint256 receivedFromB = balanceAfter - balanceBefore;

        // Pay poolA what it's owed
        IERC20(profitToken).safeTransfer(params.poolA, amountOwed);

        // Calculate and validate profit
        uint256 profit = receivedFromB > amountOwed ? receivedFromB - amountOwed : 0;
        if (profit < params.minProfit) revert InsufficientProfit(profit, params.minProfit);

        // Transfer profit to owner
        if (profit > 0) {
            IERC20(profitToken).safeTransfer(owner, profit);
        }

        emit ArbitrageExecuted(params.poolA, params.poolB, profitToken, profit);
    }

    /// @dev Second-layer callback: called by poolB. Simply pay poolB the tokens it needs.
    ///      We have these tokens from poolA's output.
    function _handlePoolBCallback(int256 amount0Delta, int256 amount1Delta) internal {
        address token0 = IUniswapV3Pool(msg.sender).token0();
        address token1 = IUniswapV3Pool(msg.sender).token1();

        if (amount0Delta > 0) {
            IERC20(token0).safeTransfer(msg.sender, uint256(amount0Delta));
        }
        if (amount1Delta > 0) {
            IERC20(token1).safeTransfer(msg.sender, uint256(amount1Delta));
        }
    }
}

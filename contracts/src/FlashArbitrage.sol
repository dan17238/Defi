// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {IUniswapV3Pool, IUniswapV3SwapCallback} from "./interfaces/IUniswapV3Pool.sol";

/// @title FlashArbitrage
/// @notice Executes atomic flash swap arbitrage across UniV3-compatible DEXes (UniV3, SushiV3, PancakeSwapV3).
/// @dev Multi-hop uses nested callbacks: pool[0].swap() → callback → pool[1].swap() → ... → pool[N-1].
///      Each callback knows its step index via abi-encoded data. The route must be circular:
///      the last pool's output token must equal what pool[0] wants back.
///      PancakeSwap V3 pools call pancakeV3SwapCallback instead of uniswapV3SwapCallback,
///      but the function signature and semantics are identical.
contract FlashArbitrage is IUniswapV3SwapCallback {
    using SafeERC20 for IERC20;

    // =========================================================================
    //                              CONSTANTS
    // =========================================================================

    uint160 private constant MIN_SQRT_RATIO_PLUS_ONE = 4295128740;
    uint160 private constant MAX_SQRT_RATIO_MINUS_ONE = 1461446703485210103287273052203988822378723970341;

    uint256 private constant MAX_HOPS = 8;

    // =========================================================================
    //                              STRUCTS
    // =========================================================================

    /// @notice Parameters for a 2-pool arbitrage (legacy, kept for backward compatibility)
    struct ArbParams {
        address poolA;
        address poolB;
        bool zeroForOne;
        int256 amountIn;
        uint256 minProfit;
    }

    /// @notice Parameters for a multi-hop arbitrage route
    struct MultiHopParams {
        address[] pools; // Ordered list of pools (length >= 2, <= MAX_HOPS)
        bool[] zeroForOne; // Swap direction for each pool
        int256 amountIn; // Amount to flash swap on pools[0]
        uint256 minProfit; // Minimum profit in the "owed" token of pools[0]
    }

    // =========================================================================
    //                              STATE
    // =========================================================================

    address public owner;
    address public pendingOwner;

    /// @dev Reentrancy + execution context
    bool private _executing;

    /// @dev 2-pool state (legacy)
    address private _arbPoolA;
    address private _arbPoolB;

    /// @dev Multi-hop state: pool addresses and directions stored during execution
    mapping(uint256 => address) private _hops;
    mapping(uint256 => bool) private _hopZeroForOne;
    uint256 private _hopCount;
    uint256 private _minProfit;
    address private _profitToken;
    uint256 private _profitBalanceBefore;

    // =========================================================================
    //                              EVENTS
    // =========================================================================

    event ArbitrageExecuted(address indexed poolFirst, address indexed poolLast, address tokenProfit, uint256 profit);

    event EmergencyWithdraw(address indexed token, uint256 amount);

    // =========================================================================
    //                              ERRORS
    // =========================================================================

    error OnlyOwner();
    error Reentrancy();
    error InvalidCallback();
    error InvalidAmount();
    error InvalidPoolPair();
    error InvalidRoute();
    error ZeroAddress();
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

    /// @notice Execute an atomic arbitrage between two UniV3 pools (legacy)
    function executeArbitrage(ArbParams calldata params) external onlyOwner {
        if (_executing) revert Reentrancy();
        if (params.amountIn <= 0) revert InvalidAmount();
        if (params.poolA == params.poolB) revert InvalidPoolPair();

        address token0A = IUniswapV3Pool(params.poolA).token0();
        address token1A = IUniswapV3Pool(params.poolA).token1();
        if (token0A != IUniswapV3Pool(params.poolB).token0() || token1A != IUniswapV3Pool(params.poolB).token1()) {
            revert InvalidPoolPair();
        }

        _sweepExecutionDust(token0A);
        if (token1A != token0A) {
            _sweepExecutionDust(token1A);
        }

        _executing = true;
        _arbPoolA = params.poolA;
        _arbPoolB = params.poolB;

        uint160 sqrtPriceLimit = params.zeroForOne ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE;

        IUniswapV3Pool(params.poolA)
            .swap(address(this), params.zeroForOne, params.amountIn, sqrtPriceLimit, abi.encode(params));

        _arbPoolA = address(0);
        _arbPoolB = address(0);
        _executing = false;
    }

    /// @notice Execute a multi-hop arbitrage across N UniV3 pools
    /// @dev The route must be circular: the token pool[0] wants back must be the
    ///      same token pool[N-1] outputs. All intermediate tokens flow automatically.
    /// @param params The multi-hop parameters
    function executeMultiHop(MultiHopParams calldata params) external onlyOwner {
        if (_executing) revert Reentrancy();
        if (params.amountIn <= 0) revert InvalidAmount();
        uint256 n = params.pools.length;
        if (n < 2 || n > MAX_HOPS) revert InvalidRoute();
        if (params.zeroForOne.length != n) revert InvalidRoute();

        address[] memory routeTokens = new address[](n * 2);
        uint256 routeTokenCount = 0;
        for (uint256 i = 0; i < n; i++) {
            address hopToken0 = IUniswapV3Pool(params.pools[i]).token0();
            address hopToken1 = IUniswapV3Pool(params.pools[i]).token1();
            if (!_containsToken(routeTokens, routeTokenCount, hopToken0)) {
                routeTokens[routeTokenCount++] = hopToken0;
            }
            if (!_containsToken(routeTokens, routeTokenCount, hopToken1)) {
                routeTokens[routeTokenCount++] = hopToken1;
            }
        }
        for (uint256 i = 0; i < routeTokenCount; i++) {
            _sweepExecutionDust(routeTokens[i]);
        }

        _executing = true;
        _hopCount = n;
        _minProfit = params.minProfit;

        address token0 = IUniswapV3Pool(params.pools[0]).token0();
        address token1 = IUniswapV3Pool(params.pools[0]).token1();
        _profitToken = params.zeroForOne[0] ? token0 : token1;
        _profitBalanceBefore = IERC20(_profitToken).balanceOf(address(this));

        for (uint256 i = 0; i < n; i++) {
            _hops[i] = params.pools[i];
            _hopZeroForOne[i] = params.zeroForOne[i];
        }

        uint160 limit = params.zeroForOne[0] ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE;

        // Initiate flash swap on pool[0]. The nested callback chain handles the rest.
        IUniswapV3Pool(params.pools[0])
            .swap(
                address(this),
                params.zeroForOne[0],
                params.amountIn,
                limit,
                abi.encode(uint256(0)) // step = 0
            );

        // Cleanup
        for (uint256 i = 0; i < n; i++) {
            _hops[i] = address(0);
            _hopZeroForOne[i] = false;
        }
        _hopCount = 0;
        _minProfit = 0;
        _profitToken = address(0);
        _profitBalanceBefore = 0;
        _executing = false;
    }

    /// @notice UniV3/SushiV3 swap callback — routes to legacy 2-pool or multi-hop handler
    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external override {
        _swapCallback(amount0Delta, amount1Delta, data);
    }

    /// @notice PancakeSwap V3 swap callback — identical semantics, different function name
    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        _swapCallback(amount0Delta, amount1Delta, data);
    }

    /// @notice Camelot V3 (Algebra) swap callback — identical semantics, different function name
    function algebraSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        _swapCallback(amount0Delta, amount1Delta, data);
    }

    function _swapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) internal {
        if (!_executing) revert InvalidCallback();

        // Multi-hop path: _hopCount > 0 means we're in executeMultiHop
        if (_hopCount > 0) {
            _handleMultiHopCallback(amount0Delta, amount1Delta, data);
            return;
        }

        // Legacy 2-pool path
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

    /// @notice Initiate 2-step ownership transfer
    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        pendingOwner = newOwner;
    }

    /// @notice Accept ownership (must be called by pendingOwner)
    function acceptOwnership() external {
        if (msg.sender != pendingOwner) revert OnlyOwner();
        owner = pendingOwner;
        pendingOwner = address(0);
    }

    function emergencyWithdraw(address token, uint256 amount) external onlyOwner {
        uint256 balance = IERC20(token).balanceOf(address(this));
        uint256 withdrawAmount = amount > balance ? balance : amount;
        if (withdrawAmount > 0) {
            IERC20(token).safeTransfer(owner, withdrawAmount);
            emit EmergencyWithdraw(token, withdrawAmount);
        }
    }

    function emergencyWithdrawETH() external onlyOwner {
        uint256 balance = address(this).balance;
        if (balance > 0) {
            (bool success,) = owner.call{value: balance}("");
            require(success, "ETH transfer failed");
            emit EmergencyWithdraw(address(0), balance);
        }
    }

    receive() external payable {}

    // =========================================================================
    //                   MULTI-HOP INTERNAL FUNCTIONS
    // =========================================================================

    /// @dev Handle callback for any step in a multi-hop route.
    ///      - Non-final step: swap received tokens on the next pool, then pay current pool
    ///      - Final step: pay current pool with tokens we have
    ///      - Step 0 (after unwind): collect profit and send to owner
    function _handleMultiHopCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) internal {
        uint256 step = abi.decode(data, (uint256));
        if (step >= _hopCount || msg.sender != _hops[step]) revert InvalidCallback();

        // Determine what we owe and what we received
        address token0 = IUniswapV3Pool(msg.sender).token0();
        address token1 = IUniswapV3Pool(msg.sender).token1();

        address tokenOwed;
        uint256 amountOwed;
        uint256 amountReceived;

        if (amount0Delta > 0) {
            tokenOwed = token0;
            amountOwed = uint256(amount0Delta);
            amountReceived = uint256(-amount1Delta);
        } else {
            tokenOwed = token1;
            amountOwed = uint256(amount1Delta);
            amountReceived = uint256(-amount0Delta);
        }

        if (step < _hopCount - 1) {
            // Continue chain: swap received tokens on next pool
            uint256 nextStep = step + 1;
            bool nextZeroForOne = _hopZeroForOne[nextStep];
            uint160 limit = nextZeroForOne ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE;

            IUniswapV3Pool(_hops[nextStep])
                .swap(address(this), nextZeroForOne, int256(amountReceived), limit, abi.encode(nextStep));

            // After nested chain returns, pay this pool
            IERC20(tokenOwed).safeTransfer(msg.sender, amountOwed);
        } else {
            // Last hop: just pay with tokens we have
            IERC20(tokenOwed).safeTransfer(msg.sender, amountOwed);
        }

        // At step 0: everything has unwound — collect profit
        if (step == 0) {
            if (tokenOwed != _profitToken) revert InvalidRoute();

            uint256 balanceAfter = IERC20(_profitToken).balanceOf(address(this));
            uint256 profit = balanceAfter > _profitBalanceBefore ? balanceAfter - _profitBalanceBefore : 0;
            if (profit < _minProfit) revert InsufficientProfit(profit, _minProfit);

            if (profit > 0) {
                IERC20(_profitToken).safeTransfer(owner, profit);
            }

            emit ArbitrageExecuted(_hops[0], _hops[_hopCount - 1], _profitToken, profit);
        }
    }

    // =========================================================================
    //                   LEGACY 2-POOL INTERNAL FUNCTIONS
    // =========================================================================

    function _handlePoolACallback(int256 amount0Delta, int256 amount1Delta, ArbParams memory params) internal {
        address token0 = IUniswapV3Pool(params.poolA).token0();
        address token1 = IUniswapV3Pool(params.poolA).token1();

        address profitToken;
        uint256 amountOwed;
        uint256 amountReceived;

        if (amount0Delta > 0) {
            profitToken = token0;
            amountOwed = uint256(amount0Delta);
            amountReceived = uint256(-amount1Delta);
        } else {
            profitToken = token1;
            amountOwed = uint256(amount1Delta);
            amountReceived = uint256(-amount0Delta);
        }

        uint256 balanceBefore = IERC20(profitToken).balanceOf(address(this));

        bool zeroForOneB = !params.zeroForOne;
        uint160 sqrtPriceLimitB = zeroForOneB ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE;

        IUniswapV3Pool(params.poolB).swap(address(this), zeroForOneB, int256(amountReceived), sqrtPriceLimitB, "");

        uint256 balanceAfter = IERC20(profitToken).balanceOf(address(this));
        uint256 receivedFromB = balanceAfter - balanceBefore;

        IERC20(profitToken).safeTransfer(params.poolA, amountOwed);

        uint256 profit = receivedFromB > amountOwed ? receivedFromB - amountOwed : 0;
        if (profit < params.minProfit) revert InsufficientProfit(profit, params.minProfit);

        if (profit > 0) {
            IERC20(profitToken).safeTransfer(owner, profit);
        }

        emit ArbitrageExecuted(params.poolA, params.poolB, profitToken, profit);
    }

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

    function _sweepExecutionDust(address token) internal {
        uint256 balance = IERC20(token).balanceOf(address(this));
        if (balance > 0) {
            IERC20(token).safeTransfer(owner, balance);
        }
    }

    function _containsToken(address[] memory tokens, uint256 length, address token) internal pure returns (bool) {
        for (uint256 i = 0; i < length; i++) {
            if (tokens[i] == token) {
                return true;
            }
        }
        return false;
    }
}

// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {IAaveV3Pool, IFlashLoanSimpleReceiver} from "./interfaces/IAaveV3Pool.sol";
import {IRadiantLendingPool, IFlashLoanReceiver} from "./interfaces/IRadiantPool.sol";
import {ISilo} from "./interfaces/ISilo.sol";
import {SwapHelper} from "./libraries/SwapHelper.sol";

/// @title FlashLiquidator
/// @notice Executes flash-loan-funded liquidations across AAVE v3, Radiant, and Silo on Arbitrum
/// @dev Implements both AAVE v3 (flashLoanSimple) and Radiant (AAVE v2 style) flash loan callbacks
contract FlashLiquidator is IFlashLoanSimpleReceiver, IFlashLoanReceiver {
    using SafeERC20 for IERC20;
    using SwapHelper for *;

    // =========================================================================
    //                              CONSTANTS
    // =========================================================================

    /// @dev AAVE v3 Pool on Arbitrum
    address public constant AAVE_V3_POOL = 0x794a61358D6845594F94dc1DB02A252b5b4814aD;

    /// @dev Radiant LendingPool on Arbitrum
    address public constant RADIANT_LENDING_POOL = 0xF4B1486DD74D07706052A33d31d7c0AAFD0659E1;

    // =========================================================================
    //                              ENUMS
    // =========================================================================

    /// @notice The lending protocol to liquidate on
    enum Protocol {
        AaveV3, // 0
        Radiant, // 1
        Silo // 2
    }

    /// @notice The flash loan provider to use
    enum FlashLoanProvider {
        AaveV3, // 0
        Radiant // 1
    }

    // =========================================================================
    //                              STRUCTS
    // =========================================================================

    /// @notice Parameters for a liquidation operation
    struct LiquidationParams {
        Protocol protocol; // Which lending protocol to liquidate on
        address collateralAsset; // The collateral to seize
        address debtAsset; // The debt to repay
        address user; // The borrower to liquidate
        uint256 debtToCover; // Amount of debt to repay
        SwapHelper.DEX swapDex; // Which DEX to swap on
        uint24 swapFee; // Uniswap V3 fee tier (ignored for Camelot)
        uint256 minProfit; // Minimum profit required (in debt asset units)
        bytes swapPath; // Optional: multi-hop swap path for Uniswap V3 (empty = single hop)
        address siloAddress; // Only used for Silo protocol liquidations
        uint256 minAmountOut; // Minimum amount from swap (slippage protection)
    }

    // =========================================================================
    //                              STATE
    // =========================================================================

    /// @notice Owner of the contract (receives profits, can withdraw)
    address public owner;

    /// @notice Pending owner for 2-step ownership transfer
    address public pendingOwner;

    /// @notice Tracks whether we are inside a flash loan to prevent reentrancy
    bool private _inFlashLoan;

    // =========================================================================
    //                              EVENTS
    // =========================================================================

    event LiquidationExecuted(
        Protocol indexed protocol,
        address indexed user,
        address collateralAsset,
        address debtAsset,
        uint256 debtRepaid,
        uint256 collateralReceived,
        uint256 profit
    );

    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);
    event EmergencyWithdraw(address indexed token, uint256 amount);

    // =========================================================================
    //                              ERRORS
    // =========================================================================

    error OnlyOwner();
    error OnlyPool();
    error OnlyDuringFlashLoan();
    error InsufficientProfit(uint256 actual, uint256 required);
    error InvalidProtocol();
    error InvalidFlashLoanProvider();
    error ZeroAddress();
    error FlashLoanReentrancy();

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

    /// @notice Execute a liquidation using an AAVE v3 flash loan
    /// @param params The liquidation parameters
    function liquidateWithAaveFlashLoan(LiquidationParams calldata params) external onlyOwner {
        if (_inFlashLoan) revert FlashLoanReentrancy();
        if (params.debtToCover == 0) revert("zero debt");

        bytes memory encodedParams = abi.encode(params);

        IAaveV3Pool(AAVE_V3_POOL).flashLoanSimple(
            address(this),
            params.debtAsset,
            params.debtToCover,
            encodedParams,
            0 // referralCode
        );
    }

    /// @notice Execute a liquidation using a Radiant flash loan
    /// @param params The liquidation parameters
    function liquidateWithRadiantFlashLoan(LiquidationParams calldata params) external onlyOwner {
        if (_inFlashLoan) revert FlashLoanReentrancy();
        if (params.debtToCover == 0) revert("zero debt");

        bytes memory encodedParams = abi.encode(params);

        address[] memory assets = new address[](1);
        assets[0] = params.debtAsset;

        uint256[] memory amounts = new uint256[](1);
        amounts[0] = params.debtToCover;

        uint256[] memory modes = new uint256[](1);
        modes[0] = 0; // No debt (full repayment required)

        IRadiantLendingPool(RADIANT_LENDING_POOL).flashLoan(
            address(this),
            assets,
            amounts,
            modes,
            address(this), // onBehalfOf
            encodedParams,
            0 // referralCode
        );
    }

    /// @notice Execute a Silo liquidation funded by an AAVE v3 flash loan
    /// @dev Silo does not have its own flash loan; we borrow via AAVE v3
    function liquidateSiloWithAaveFlashLoan(LiquidationParams calldata) external view onlyOwner {
        revert("Silo not yet supported");
    }

    // =========================================================================
    //                      FLASH LOAN CALLBACKS
    // =========================================================================

    /// @notice AAVE v3 flash loan callback (flashLoanSimple)
    /// @dev Called by the AAVE v3 Pool after funds are transferred
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external override(IFlashLoanSimpleReceiver) returns (bool) {
        // Security: only the AAVE v3 pool can call this
        if (msg.sender != AAVE_V3_POOL) revert OnlyPool();
        // Security: only this contract should have initiated the flash loan
        if (initiator != address(this)) revert OnlyPool();

        _handleFlashLoanCallback(asset, amount, premium, AAVE_V3_POOL, params);
        return true;
    }

    /// @notice Radiant (AAVE v2 style) flash loan callback
    /// @dev Called by the Radiant LendingPool after funds are transferred
    function executeOperation(
        address[] calldata assets,
        uint256[] calldata amounts,
        uint256[] calldata premiums,
        address initiator,
        bytes calldata params
    ) external override(IFlashLoanReceiver) returns (bool) {
        // Security: only the Radiant lending pool can call this
        if (msg.sender != RADIANT_LENDING_POOL) revert OnlyPool();
        if (initiator != address(this)) revert OnlyPool();

        _handleFlashLoanCallback(assets[0], amounts[0], premiums[0], RADIANT_LENDING_POOL, params);
        return true;
    }

    // =========================================================================
    //                         ADMIN FUNCTIONS
    // =========================================================================

    /// @notice Initiate a 2-step ownership transfer
    /// @param newOwner The address of the new owner
    function transferOwnership(address newOwner) external onlyOwner {
        if (newOwner == address(0)) revert ZeroAddress();
        pendingOwner = newOwner;
    }

    /// @notice Accept ownership (must be called by the pending owner)
    function acceptOwnership() external {
        if (msg.sender != pendingOwner) revert OnlyOwner();
        emit OwnershipTransferred(owner, pendingOwner);
        owner = pendingOwner;
        pendingOwner = address(0);
    }

    /// @notice Emergency withdraw any ERC-20 token stuck in the contract
    /// @param token The address of the token to withdraw
    /// @param amount The amount to withdraw (use type(uint256).max for full balance)
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

    /// @dev Shared logic for both AAVE v3 and Radiant flash loan callbacks
    /// @param asset The flash-loaned asset (debt token)
    /// @param amount The flash-loaned amount
    /// @param premium The flash loan fee
    /// @param pool The lending pool address to approve for repayment
    /// @param params The encoded LiquidationParams
    function _handleFlashLoanCallback(
        address asset,
        uint256 amount,
        uint256 premium,
        address pool,
        bytes calldata params
    ) internal {
        _inFlashLoan = true;

        LiquidationParams memory liqParams = abi.decode(params, (LiquidationParams));

        require(asset == liqParams.debtAsset, "asset mismatch");

        // Calculate total owed to the flash loan pool up front so swap slippage
        // checks can use the real callback premium instead of a Rust-side guess.
        uint256 totalOwed = amount + premium;

        // Execute the liquidation and swap collateral back to the debt token
        uint256 collateralReceived = _executeLiquidation(liqParams);
        _swapCollateralToDebt(liqParams, collateralReceived, totalOwed);

        // Approve the pool to pull back the owed amount
        IERC20(asset).forceApprove(pool, totalOwed);

        // Calculate profit: our balance minus what the pool will pull
        uint256 balance = IERC20(asset).balanceOf(address(this));
        uint256 profit = balance > totalOwed ? balance - totalOwed : 0;

        if (profit < liqParams.minProfit) revert InsufficientProfit(profit, liqParams.minProfit);

        // Transfer profit to owner
        if (profit > 0) {
            IERC20(asset).safeTransfer(owner, profit);
        }

        // Transfer any residual collateral tokens (in case swap was partial)
        _sweepResidual(liqParams.collateralAsset, asset);

        // Reset reentrancy flag AFTER all external transfers
        _inFlashLoan = false;

        emit LiquidationExecuted(
            liqParams.protocol, liqParams.user, liqParams.collateralAsset, liqParams.debtAsset,
            amount, collateralReceived, profit
        );
    }

    /// @dev Transfer any residual tokens to the owner (e.g. leftover collateral after swap)
    /// @param token The token to sweep
    /// @param exclude Skip if this token equals `token` (already handled as the debt asset)
    function _sweepResidual(address token, address exclude) internal {
        if (token == exclude) return;
        uint256 residual = IERC20(token).balanceOf(address(this));
        if (residual > 0) {
            IERC20(token).safeTransfer(owner, residual);
        }
    }

    /// @dev Execute a liquidation on the specified protocol
    /// @param params The liquidation parameters
    /// @return collateralReceived The amount of collateral tokens received
    function _executeLiquidation(LiquidationParams memory params) internal returns (uint256 collateralReceived) {
        uint256 collateralBefore = IERC20(params.collateralAsset).balanceOf(address(this));

        if (params.protocol == Protocol.AaveV3) {
            _liquidateAaveV3(params);
        } else if (params.protocol == Protocol.Radiant) {
            _liquidateRadiant(params);
        } else if (params.protocol == Protocol.Silo) {
            _liquidateSilo(params);
        } else {
            revert InvalidProtocol();
        }

        collateralReceived = IERC20(params.collateralAsset).balanceOf(address(this)) - collateralBefore;
    }

    /// @dev Execute a liquidation on AAVE v3
    function _liquidateAaveV3(LiquidationParams memory params) internal {
        // Approve the AAVE v3 pool to spend the debt tokens for the liquidation call
        IERC20(params.debtAsset).forceApprove(AAVE_V3_POOL, params.debtToCover);

        IAaveV3Pool(AAVE_V3_POOL).liquidationCall(
            params.collateralAsset,
            params.debtAsset,
            params.user,
            params.debtToCover,
            false // receive underlying, not aTokens
        );

        // Reset dangling approval
        IERC20(params.debtAsset).forceApprove(AAVE_V3_POOL, 0);
    }

    /// @dev Execute a liquidation on Radiant
    function _liquidateRadiant(LiquidationParams memory params) internal {
        // Approve the Radiant pool to spend the debt tokens for the liquidation call
        IERC20(params.debtAsset).forceApprove(RADIANT_LENDING_POOL, params.debtToCover);

        IRadiantLendingPool(RADIANT_LENDING_POOL).liquidationCall(
            params.collateralAsset,
            params.debtAsset,
            params.user,
            params.debtToCover,
            false // receive underlying, not rTokens
        );

        // Reset dangling approval
        IERC20(params.debtAsset).forceApprove(RADIANT_LENDING_POOL, 0);
    }

    /// @dev Execute a liquidation on Silo Finance
    function _liquidateSilo(LiquidationParams memory params) internal {
        if (params.siloAddress == address(0)) revert ZeroAddress();

        // For Silo, we need to approve the debt asset to the silo first,
        // then call repay + liquidate. Silo's liquidate pulls repayment
        // from the caller and sends collateral back.
        IERC20(params.debtAsset).forceApprove(params.siloAddress, params.debtToCover);

        // Silo liquidation: repay debt and receive collateral
        ISilo(params.siloAddress).liquidate(params.user);
    }

    /// @dev Swap received collateral back to the debt asset
    /// @param params The liquidation parameters
    /// @param collateralAmount The amount of collateral to swap
    /// @return amountOut The amount of debt tokens received
    function _swapCollateralToDebt(
        LiquidationParams memory params,
        uint256 collateralAmount,
        uint256 totalOwed
    )
        internal
        returns (uint256 amountOut)
    {
        // If collateral == debt, no swap needed
        if (params.collateralAsset == params.debtAsset) {
            return collateralAmount;
        }

        // If no collateral received, nothing to swap
        if (collateralAmount == 0) {
            return 0;
        }

        uint256 minAmountOut = params.minAmountOut > totalOwed ? params.minAmountOut : totalOwed;

        // Use multi-hop path if provided, otherwise single-hop
        if (params.swapPath.length > 0 && params.swapDex == SwapHelper.DEX.UniswapV3) {
            amountOut = SwapHelper.swapUniswapV3MultiHop(params.swapPath, collateralAmount, minAmountOut);
        } else {
            amountOut = SwapHelper.swap(
                params.swapDex,
                params.collateralAsset,
                params.debtAsset,
                collateralAmount,
                minAmountOut,
                params.swapFee
            );
        }
    }
}

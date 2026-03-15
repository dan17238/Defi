// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title IAaveV3Pool
/// @notice Interface for the AAVE v3 Pool on Arbitrum
interface IAaveV3Pool {
    /// @notice Executes a simple flash loan (single asset)
    /// @param receiverAddress The address that will receive the flash-loaned assets
    /// @param asset The address of the underlying asset to flash loan
    /// @param amount The amount to flash loan
    /// @param params Arbitrary bytes to pass to the receiver's executeOperation
    /// @param referralCode Referral code for tracking (use 0 if none)
    function flashLoanSimple(
        address receiverAddress,
        address asset,
        uint256 amount,
        bytes calldata params,
        uint16 referralCode
    ) external;

    /// @notice Liquidates an undercollateralized position
    /// @param collateralAsset The address of the collateral asset to receive
    /// @param debtAsset The address of the debt asset to repay
    /// @param user The address of the borrower to liquidate
    /// @param debtToCover The amount of debt to repay
    /// @param receiveAToken True to receive aTokens, false to receive underlying
    function liquidationCall(
        address collateralAsset,
        address debtAsset,
        address user,
        uint256 debtToCover,
        bool receiveAToken
    ) external;

    /// @notice Returns the user account data across all reserves
    /// @param user The address of the user
    /// @return totalCollateralBase The total collateral in the base currency of the price oracle
    /// @return totalDebtBase The total debt in the base currency of the price oracle
    /// @return availableBorrowsBase The borrowing power left in the base currency
    /// @return currentLiquidationThreshold The liquidation threshold of the user
    /// @return ltv The loan-to-value of the user
    /// @return healthFactor The current health factor of the user
    function getUserAccountData(address user)
        external
        view
        returns (
            uint256 totalCollateralBase,
            uint256 totalDebtBase,
            uint256 availableBorrowsBase,
            uint256 currentLiquidationThreshold,
            uint256 ltv,
            uint256 healthFactor
        );

    /// @notice Returns the normalized income of a reserve
    /// @param asset The address of the underlying asset
    /// @return The reserve's normalized income
    function getReserveNormalizedIncome(address asset) external view returns (uint256);

    /// @notice Returns the normalized variable debt of a reserve
    /// @param asset The address of the underlying asset
    /// @return The reserve's normalized variable debt
    function getReserveNormalizedVariableDebt(address asset) external view returns (uint256);

    /// @notice Returns the constant for the flash loan premium (total), in bps
    /// @return The flash loan premium total
    function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);
}

/// @title IPoolDataProvider
/// @notice Interface for AAVE v3 Pool Data Provider
interface IPoolDataProvider {
    /// @notice Returns the user reserve data
    /// @param asset The address of the underlying asset
    /// @param user The address of the user
    /// @return currentATokenBalance The current aToken balance
    /// @return currentStableDebt The current stable debt
    /// @return currentVariableDebt The current variable debt
    /// @return principalStableDebt The principal stable debt
    /// @return scaledVariableDebt The scaled variable debt
    /// @return stableBorrowRate The stable borrow rate
    /// @return liquidityRate The liquidity rate
    /// @return stableRateLastUpdated The timestamp of the last stable rate update
    /// @return usageAsCollateralEnabled True if the user is using the asset as collateral
    function getUserReserveData(address asset, address user)
        external
        view
        returns (
            uint256 currentATokenBalance,
            uint256 currentStableDebt,
            uint256 currentVariableDebt,
            uint256 principalStableDebt,
            uint256 scaledVariableDebt,
            uint256 stableBorrowRate,
            uint256 liquidityRate,
            uint40 stableRateLastUpdated,
            bool usageAsCollateralEnabled
        );

    /// @notice Returns the list of initialized reserves
    /// @return An array of reserve token addresses
    function getAllReservesTokens() external view returns (TokenData[] memory);

    struct TokenData {
        string symbol;
        address tokenAddress;
    }
}

/// @title IFlashLoanSimpleReceiver
/// @notice Interface for the AAVE v3 flash loan simple receiver callback
interface IFlashLoanSimpleReceiver {
    /// @notice Callback executed by the Pool after a flash loan is issued
    /// @param asset The address of the flash-loaned asset
    /// @param amount The amount flash-loaned
    /// @param premium The fee charged for the flash loan
    /// @param initiator The address that initiated the flash loan
    /// @param params Arbitrary bytes passed from the flash loan call
    /// @return True if the execution succeeded
    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external returns (bool);
}

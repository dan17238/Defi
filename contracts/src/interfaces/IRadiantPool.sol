// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title IRadiantLendingPool
/// @notice Interface for the Radiant Capital LendingPool (AAVE v2 fork) on Arbitrum
interface IRadiantLendingPool {
    /// @notice Executes a flash loan with multiple assets
    /// @param receiverAddress The address that will receive the flash-loaned assets
    /// @param assets The addresses of the underlying assets to flash loan
    /// @param amounts The amounts to flash loan per asset
    /// @param modes The flash loan modes (0 = no debt, 1 = stable, 2 = variable)
    /// @param onBehalfOf The address that will incur the debt (if mode != 0)
    /// @param params Arbitrary bytes to pass to the receiver
    /// @param referralCode Referral code for tracking
    function flashLoan(
        address receiverAddress,
        address[] calldata assets,
        uint256[] calldata amounts,
        uint256[] calldata modes,
        address onBehalfOf,
        bytes calldata params,
        uint16 referralCode
    ) external;

    /// @notice Liquidates an undercollateralized position
    /// @param collateralAsset The address of the collateral asset to receive
    /// @param debtAsset The address of the debt asset to repay
    /// @param user The address of the borrower to liquidate
    /// @param debtToCover The amount of debt to repay
    /// @param receiveAToken True to receive rTokens, false to receive underlying
    function liquidationCall(
        address collateralAsset,
        address debtAsset,
        address user,
        uint256 debtToCover,
        bool receiveAToken
    ) external;

    /// @notice Returns the user account data across all reserves
    /// @param user The address of the user
    /// @return totalCollateralETH The total collateral in ETH
    /// @return totalDebtETH The total debt in ETH
    /// @return availableBorrowsETH The borrowing power left in ETH
    /// @return currentLiquidationThreshold The liquidation threshold of the user
    /// @return ltv The loan-to-value of the user
    /// @return healthFactor The current health factor of the user
    function getUserAccountData(address user)
        external
        view
        returns (
            uint256 totalCollateralETH,
            uint256 totalDebtETH,
            uint256 availableBorrowsETH,
            uint256 currentLiquidationThreshold,
            uint256 ltv,
            uint256 healthFactor
        );

    /// @notice Returns the constant for the flash loan premium, in bps
    /// @return The flash loan premium
    function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint256);
}

/// @title IFlashLoanReceiver
/// @notice Interface for the Radiant (AAVE v2 style) flash loan receiver callback
interface IFlashLoanReceiver {
    /// @notice Callback executed by the LendingPool after a flash loan
    /// @param assets The addresses of the flash-loaned assets
    /// @param amounts The amounts flash-loaned per asset
    /// @param premiums The fees charged per asset
    /// @param initiator The address that initiated the flash loan
    /// @param params Arbitrary bytes passed from the flash loan call
    /// @return True if the execution succeeded
    function executeOperation(
        address[] calldata assets,
        uint256[] calldata amounts,
        uint256[] calldata premiums,
        address initiator,
        bytes calldata params
    ) external returns (bool);
}

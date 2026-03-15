// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title ISilo
/// @notice Interface for Silo Finance lending markets on Arbitrum
interface ISilo {
    /// @notice Liquidate an undercollateralized position
    /// @param _user The address of the borrower to liquidate
    /// @return receivedCollaterals Array of collateral assets received
    /// @return shareAmountsToRepay Array of share amounts to repay
    function liquidate(address _user) external returns (address[] memory receivedCollaterals, uint256[] memory shareAmountsToRepay);

    /// @notice Deposit assets to the silo
    /// @param _amount Amount to deposit
    /// @param _collateralOnly True if this deposit is collateral-only (non-borrowable)
    /// @return collateralAmount The amount of collateral token minted
    /// @return collateralShare The share amount of collateral token minted
    function deposit(uint256 _amount, bool _collateralOnly)
        external
        returns (uint256 collateralAmount, uint256 collateralShare);

    /// @notice Withdraw assets from the silo
    /// @param _amount Amount to withdraw
    /// @param _collateralOnly True if withdrawing collateral-only deposit
    /// @return withdrawnAmount The actual amount withdrawn
    /// @return withdrawnShare The share amount burned
    function withdraw(uint256 _amount, bool _collateralOnly)
        external
        returns (uint256 withdrawnAmount, uint256 withdrawnShare);

    /// @notice Borrow assets from the silo
    /// @param _amount Amount to borrow
    /// @return debtAmount The amount of debt incurred
    /// @return debtShare The share amount of debt token minted
    function borrow(uint256 _amount) external returns (uint256 debtAmount, uint256 debtShare);

    /// @notice Repay borrowed assets
    /// @param _amount Amount to repay
    /// @return repaidAmount The actual amount repaid
    /// @return repaidShare The share amount of debt token burned
    function repay(uint256 _amount) external returns (uint256 repaidAmount, uint256 repaidShare);

    /// @notice Returns the underlying asset of the silo
    /// @return The address of the underlying asset
    function asset() external view returns (address);

    /// @notice Returns the silo's utilization data
    /// @return The silo's interest rate model data
    function utilizationData() external view returns (UtilizationData memory);

    struct UtilizationData {
        uint256 totalDeposits;
        uint256 totalBorrowAmount;
        uint64 interestRateTimestamp;
    }
}

/// @title ISiloRepository
/// @notice Interface for the Silo Repository that manages all silo markets
interface ISiloRepository {
    /// @notice Returns the silo address for a given asset
    /// @param _asset The address of the underlying asset
    /// @return The silo address for that asset
    function getSilo(address _asset) external view returns (address);

    /// @notice Checks if a user's position in a silo is solvent
    /// @param _silo The address of the silo
    /// @param _user The address of the user
    /// @return True if the user is solvent (health factor >= 1)
    function isSolvent(address _silo, address _user) external view returns (bool);

    /// @notice Returns the bridge asset address (the common token, usually ETH/WETH)
    /// @return The bridge asset address
    function bridgeAsset() external view returns (address);

    /// @notice Returns all active silos
    /// @return Array of silo addresses
    function getSilos() external view returns (address[] memory);
}

/// @title ISiloLens
/// @notice Interface for reading Silo Finance position data
interface ISiloLens {
    /// @notice Returns whether a user has a healthy position (is solvent)
    /// @param _silo The address of the silo
    /// @param _user The address of the user
    /// @return True if the user is solvent
    function isSolvent(address _silo, address _user) external view returns (bool);

    /// @notice Returns the user's collateral balance in the silo
    /// @param _silo The address of the silo
    /// @param _user The address of the user
    /// @return The user's collateral balance
    function collateralBalanceOfUnderlying(address _silo, address _user) external view returns (uint256);

    /// @notice Returns the user's debt balance in the silo
    /// @param _silo The address of the silo
    /// @param _user The address of the user
    /// @return The user's debt balance
    function debtBalanceOfUnderlying(address _silo, address _user) external view returns (uint256);

    /// @notice Returns the total deposits in the silo
    /// @param _silo The address of the silo
    /// @return The total deposits with interest
    function totalDepositsWithInterest(address _silo) external view returns (uint256);

    /// @notice Returns the total borrows in the silo
    /// @param _silo The address of the silo
    /// @return The total borrows with interest
    function totalBorrowAmountWithInterest(address _silo) external view returns (uint256);

    /// @notice Returns the user's LTV (Loan-to-Value)
    /// @param _silo The address of the silo
    /// @param _user The address of the user
    /// @return The user's LTV in 18-decimal precision
    function getUserLTV(address _silo, address _user) external view returns (uint256);

    /// @notice Returns the maximum LTV for a silo
    /// @param _silo The address of the silo
    /// @return The maximum LTV in 18-decimal precision
    function getUserMaximumLTV(address _silo, address _user) external view returns (uint256);

    /// @notice Calculates the exact liquidation amounts for a user
    /// @param _silo The address of the silo
    /// @param _user The address of the user
    /// @return collateralToLiquidate The collateral amount that will be seized
    /// @return debtToRepay The debt amount that needs to be repaid
    function calculateExactLiquidationAmounts(address _silo, address _user)
        external
        view
        returns (uint256 collateralToLiquidate, uint256 debtToRepay);
}

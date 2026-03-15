use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;

use crate::protocols::LiquidationOpportunity;

// --------------------------------------------------------------------------
// FlashLiquidator contract interface
// --------------------------------------------------------------------------

sol! {
    /// Interface for the on-chain FlashLiquidator contract that performs
    /// AAVE v3 flash loans and liquidations atomically.
    #[sol(rpc)]
    interface IFlashLiquidator {
        /// Execute a flash-loan-based liquidation.
        ///
        /// The contract will:
        /// 1. Take a flash loan of the debt asset from AAVE v3
        /// 2. Call liquidationCall on the target protocol
        /// 3. Swap received collateral back to the debt asset
        /// 4. Repay the flash loan + premium
        /// 5. Transfer profit to the caller
        function executeLiquidation(
            address protocol,
            address collateralAsset,
            address debtAsset,
            address user,
            uint256 debtToCover,
            bool receiveAToken
        ) external;

        /// Execute a batch of liquidations in a single transaction.
        function executeBatchLiquidation(
            LiquidationParams[] calldata params
        ) external;

        struct LiquidationParams {
            address protocol;
            address collateralAsset;
            address debtAsset;
            address user;
            uint256 debtToCover;
            bool receiveAToken;
        }
    }

    /// AAVE v3 flash loan interface for encoding inner params.
    interface IFlashLoanSimple {
        function flashLoanSimple(
            address receiverAddress,
            address asset,
            uint256 amount,
            bytes calldata params,
            uint16 referralCode
        ) external;
    }
}

/// Encode calldata for a single flash liquidation via the FlashLiquidator contract.
pub fn encode_flash_liquidation(
    opportunity: &LiquidationOpportunity,
    _flash_liquidator: Address,
) -> Bytes {
    // Map protocol name to on-chain protocol registry address.
    // In production, this would be read from the FlashLiquidator contract or config.
    let protocol_address = match opportunity.protocol.as_str() {
        "aave_v3" => {
            // AAVE v3 Pool on Arbitrum
            "0x794a61358D6845594F94dc1DB02A252b5b4814aD"
                .parse::<Address>()
                .expect("valid aave v3 pool address")
        }
        "radiant" => {
            // Radiant LendingPool on Arbitrum
            "0xF4B1486DD74D07706052A33d31d7c0AAFD0659E1"
                .parse::<Address>()
                .expect("valid radiant pool address")
        }
        _ => Address::ZERO,
    };

    let call = IFlashLiquidator::executeLiquidationCall {
        protocol: protocol_address,
        collateralAsset: opportunity.collateral_asset,
        debtAsset: opportunity.debt_asset,
        user: opportunity.user,
        debtToCover: opportunity.debt_to_cover,
        receiveAToken: false,
    };

    Bytes::from(call.abi_encode())
}

/// Encode calldata for a batch of flash liquidations.
pub fn encode_batch_flash_liquidation(
    opportunities: &[LiquidationOpportunity],
) -> Bytes {
    let params: Vec<IFlashLiquidator::LiquidationParams> = opportunities
        .iter()
        .map(|opp| {
            let protocol_address = match opp.protocol.as_str() {
                "aave_v3" => "0x794a61358D6845594F94dc1DB02A252b5b4814aD"
                    .parse::<Address>()
                    .expect("valid aave v3 pool address"),
                "radiant" => "0xF4B1486DD74D07706052A33d31d7c0AAFD0659E1"
                    .parse::<Address>()
                    .expect("valid radiant pool address"),
                _ => Address::ZERO,
            };

            IFlashLiquidator::LiquidationParams {
                protocol: protocol_address,
                collateralAsset: opp.collateral_asset,
                debtAsset: opp.debt_asset,
                user: opp.user,
                debtToCover: opp.debt_to_cover,
                receiveAToken: false,
            }
        })
        .collect();

    let call = IFlashLiquidator::executeBatchLiquidationCall { params };
    Bytes::from(call.abi_encode())
}

/// Encode parameters for an AAVE v3 flashLoanSimple call.
///
/// This is used to construct the inner flash loan request that the
/// FlashLiquidator contract will execute.
pub fn encode_flash_loan_simple(
    receiver: Address,
    asset: Address,
    amount: U256,
    params: Bytes,
) -> Bytes {
    let call = IFlashLoanSimple::flashLoanSimpleCall {
        receiverAddress: receiver,
        asset,
        amount,
        params: params.to_vec().into(),
        referralCode: 0,
    };
    Bytes::from(call.abi_encode())
}

/// Encode the inner liquidation parameters that are passed as the `params`
/// field of the flash loan callback.
///
/// Format: abi.encode(protocol, collateralAsset, user, debtToCover)
pub fn encode_inner_liquidation_params(
    protocol: Address,
    collateral_asset: Address,
    user: Address,
    debt_to_cover: U256,
) -> Bytes {
    use alloy::sol_types::SolValue;
    let encoded = (protocol, collateral_asset, user, debt_to_cover).abi_encode_packed();
    Bytes::from(encoded)
}

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;

use crate::protocols::LiquidationOpportunity;

// ---------------------------------------------------------------------------
// On-chain FlashLiquidator ABI (must match FlashLiquidator.sol exactly)
// ---------------------------------------------------------------------------

sol! {
    #[sol(rpc)]
    interface IFlashLiquidator {
        /// Matches FlashLiquidator.Protocol enum
        /// 0 = AaveV3, 1 = Radiant, 2 = Silo

        /// Matches SwapHelper.DEX enum
        /// 0 = UniswapV3, 1 = Camelot

        /// Matches FlashLiquidator.LiquidationParams struct exactly
        struct LiquidationParams {
            uint8 protocol;
            address collateralAsset;
            address debtAsset;
            address user;
            uint256 debtToCover;
            uint8 swapDex;          // 0 = UniswapV3, 1 = Camelot
            uint24 swapFee;         // Uniswap V3 fee tier (500, 3000, 10000)
            uint256 minProfit;      // minimum profit in debt asset units
            bytes swapPath;         // multi-hop path (empty = single hop)
            address siloAddress;    // only for Silo liquidations
            uint256 minAmountOut;   // minimum amount from swap (slippage protection)
        }

        /// Execute a liquidation using an AAVE v3 flash loan
        function liquidateWithAaveFlashLoan(LiquidationParams calldata params) external;

        /// Execute a liquidation using a Radiant flash loan
        function liquidateWithRadiantFlashLoan(LiquidationParams calldata params) external;

        /// Execute a Silo liquidation funded by an AAVE v3 flash loan
        function liquidateSiloWithAaveFlashLoan(LiquidationParams calldata params) external;
    }
}

// ---------------------------------------------------------------------------
// Protocol/DEX enum mapping
// ---------------------------------------------------------------------------

/// Map protocol name to on-chain enum value.
fn protocol_id(name: &str) -> u8 {
    match name {
        "aave_v3" => 0,  // Protocol.AaveV3
        "radiant" => 1,  // Protocol.Radiant
        "silo" => 2,     // Protocol.Silo
        _ => 0,
    }
}

/// Common Uniswap V3 fee tiers.
pub const FEE_LOW: u32 = 500;      // 0.05% — stablecoin pairs
pub const FEE_MEDIUM: u32 = 3000;  // 0.30% — most pairs
pub const FEE_HIGH: u32 = 10000;   // 1.00% — exotic pairs

/// DEX selection.
pub const DEX_UNISWAP_V3: u8 = 0;
pub const DEX_CAMELOT: u8 = 1;

// ---------------------------------------------------------------------------
// Swap route selection
// ---------------------------------------------------------------------------

/// Well-known token addresses on Arbitrum.
pub mod tokens {
    use alloy::primitives::{address, Address};

    pub const WETH: Address = address!("82aF49447D8a07e3bd95BD0d56f35241523fBab1");
    pub const USDC: Address = address!("af88d065e77c8cC2239327C5EDb3A432268e5831");
    pub const USDC_E: Address = address!("FF970A61A04b1cA14834A43f5dE4533eBDDB5CC8");
    pub const USDT: Address = address!("Fd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9");
    pub const WBTC: Address = address!("2f2a2543B76A4166549F7aaB2e75Bef0aefC5B0f");
    pub const ARB: Address = address!("912CE59144191C1204E64559FE8253a0e49E6548");
    pub const DAI: Address = address!("DA10009cBd5D07dd0CeCc66161FC93D7c9000da1");
    pub const LINK: Address = address!("f97f4df75117a78c1A5a0DBb814Af92458539FB4");
    pub const GMX: Address = address!("fc5A1A6EB076a2C7aD06eD22C90d7E710E35ad0a");
    pub const WSTETH: Address = address!("5979D7b546E38E9Ab5011956dEa6F53c2BA11622");
    pub const MAGIC: Address = address!("539bdE0d7Dbd336b79148AA742883198BBF60342");
    pub const FRAX: Address = address!("17FC002b466eEc40DaE837Fc4bE5c67993ddBd6F");

    /// Token info: (decimals, approximate USD price).
    /// Price is a rough estimate used for comparison/sorting, not for exact profit.
    /// ETH price is fetched from Chainlink and cached; others use static estimates.
    pub fn token_info(addr: Address) -> (u8, f64) {
        let eth_price = crate::protocols::radiant::CACHED_ETH_PRICE_CENTS
            .load(std::sync::atomic::Ordering::Relaxed) as f64 / 100.0;
        let eth_price = if eth_price > 100.0 { eth_price } else { 3500.0 };

        match addr {
            a if a == WETH => (18, eth_price),
            a if a == WSTETH => (18, eth_price * 1.05), // ~5% premium over ETH
            a if a == WBTC => (8, 95_000.0),
            a if a == USDC => (6, 1.0),
            a if a == USDC_E => (6, 1.0),
            a if a == USDT => (6, 1.0),
            a if a == DAI => (18, 1.0),
            a if a == FRAX => (18, 1.0),
            a if a == ARB => (18, 1.1),
            a if a == LINK => (18, 18.0),
            a if a == GMX => (18, 30.0),
            a if a == MAGIC => (18, 0.5),
            _ => (18, 1.0), // unknown 18-decimal token, assume $1 (conservative)
        }
    }

    /// Convert raw token amount to approximate USD value.
    pub fn token_value_usd(amount: alloy::primitives::U256, addr: Address) -> f64 {
        let (decimals, price) = token_info(addr);
        let raw = amount.saturating_to::<u128>() as f64;
        (raw / 10f64.powi(decimals as i32)) * price
    }

    /// How many raw token units equal ~$1.
    pub fn one_dollar_in_tokens(addr: Address) -> alloy::primitives::U256 {
        let (decimals, price) = token_info(addr);
        let price = if price > 0.0 { price } else { 1.0 };
        let units = 10f64.powi(decimals as i32) / price;
        alloy::primitives::U256::from(units as u128)
    }
}

/// Determine the best swap fee tier for a collateral→debt pair.
/// Uses lower fees for stablecoin pairs, higher fees for volatile pairs.
fn select_swap_fee(collateral: Address, debt: Address) -> u32 {
    let stables = [tokens::USDC, tokens::USDC_E, tokens::USDT, tokens::DAI];
    let is_col_stable = stables.contains(&collateral);
    let is_debt_stable = stables.contains(&debt);

    if is_col_stable && is_debt_stable {
        FEE_LOW // 0.05% for stable-stable
    } else if collateral == tokens::WETH || debt == tokens::WETH {
        FEE_MEDIUM // 0.30% for ETH pairs
    } else {
        FEE_HIGH // 1.00% for exotic pairs
    }
}

/// Build a Uniswap V3 multi-hop path if direct swap isn't optimal.
/// Returns empty bytes for single-hop (most common).
/// For exotic pairs (e.g., ARB→USDC), routes through WETH: ARB→WETH→USDC.
fn build_swap_path(collateral: Address, debt: Address) -> Bytes {
    let majors = [
        tokens::WETH,
        tokens::USDC,
        tokens::USDC_E,
        tokens::USDT,
        tokens::WBTC,
        tokens::DAI,
    ];

    // If either token is a major, single hop is fine
    if majors.contains(&collateral) || majors.contains(&debt) {
        return Bytes::new(); // empty = single hop
    }

    // For exotic collateral, route through WETH
    // Uniswap V3 path encoding: token0 (20 bytes) + fee (3 bytes) + token1 (20 bytes) + fee (3 bytes) + token2 (20 bytes)
    let fee1 = FEE_HIGH; // exotic → WETH
    let fee2 = select_swap_fee(tokens::WETH, debt); // WETH → debt

    let mut path = Vec::with_capacity(66);
    path.extend_from_slice(collateral.as_slice()); // 20 bytes
    path.extend_from_slice(&fee1.to_be_bytes()[1..]); // 3 bytes (uint24)
    path.extend_from_slice(tokens::WETH.as_slice()); // 20 bytes
    path.extend_from_slice(&fee2.to_be_bytes()[1..]); // 3 bytes (uint24)
    path.extend_from_slice(debt.as_slice()); // 20 bytes

    Bytes::from(path)
}

// ---------------------------------------------------------------------------
// Public encoding functions
// ---------------------------------------------------------------------------

/// Build the full `LiquidationParams` for a given opportunity.
pub fn build_liquidation_params(
    opportunity: &LiquidationOpportunity,
    min_profit: U256,
) -> IFlashLiquidator::LiquidationParams {
    let proto = protocol_id(&opportunity.protocol);
    let fee = select_swap_fee(opportunity.collateral_asset, opportunity.debt_asset);
    let path = build_swap_path(opportunity.collateral_asset, opportunity.debt_asset);

    // The swap converts collateral -> debt tokens. We need enough debt tokens back
    // to repay the flash loan (debt_to_cover) plus the flash loan premium (0.09%).
    // Allow 2% slippage on top of that.
    let premium = opportunity.debt_to_cover * U256::from(9) / U256::from(10000); // 0.09%
    let needed = opportunity.debt_to_cover + premium;
    let min_amount_out = needed * U256::from(98) / U256::from(100); // 2% slippage

    IFlashLiquidator::LiquidationParams {
        protocol: proto,
        collateralAsset: opportunity.collateral_asset,
        debtAsset: opportunity.debt_asset,
        user: opportunity.user,
        debtToCover: opportunity.debt_to_cover,
        swapDex: DEX_UNISWAP_V3,
        swapFee: alloy::primitives::Uint::from(fee),
        minProfit: min_profit,
        swapPath: path,
        siloAddress: Address::ZERO,
        minAmountOut: min_amount_out,
    }
}

/// Encode calldata for `liquidateWithAaveFlashLoan(LiquidationParams)`.
/// Used for AAVE v3 and Silo liquidations.
pub fn encode_aave_flash_liquidation(
    opportunity: &LiquidationOpportunity,
    min_profit: U256,
) -> Bytes {
    let params = build_liquidation_params(opportunity, min_profit);
    let call = IFlashLiquidator::liquidateWithAaveFlashLoanCall { params };
    Bytes::from(call.abi_encode())
}

/// Encode calldata for `liquidateWithRadiantFlashLoan(LiquidationParams)`.
pub fn encode_radiant_flash_liquidation(
    opportunity: &LiquidationOpportunity,
    min_profit: U256,
) -> Bytes {
    let params = build_liquidation_params(opportunity, min_profit);
    let call = IFlashLiquidator::liquidateWithRadiantFlashLoanCall { params };
    Bytes::from(call.abi_encode())
}

/// Choose the best flash loan source and encode the transaction.
/// - AAVE v3 liquidations → use AAVE flash loan
/// - Radiant liquidations → use Radiant flash loan (avoids same-pool reentrancy)
/// - Silo liquidations → use AAVE flash loan
pub fn encode_flash_liquidation(
    opportunity: &LiquidationOpportunity,
    _flash_liquidator: Address,
    min_profit: U256,
) -> Bytes {
    match opportunity.protocol.as_str() {
        "radiant" => encode_radiant_flash_liquidation(opportunity, min_profit),
        _ => encode_aave_flash_liquidation(opportunity, min_profit),
    }
}

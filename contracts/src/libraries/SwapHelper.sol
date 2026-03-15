// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @title ISwapRouter
/// @notice Interface for Uniswap V3 SwapRouter on Arbitrum
interface ISwapRouter {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }

    /// @notice Swaps amountIn of one token for as much as possible of another token
    /// @param params The swap parameters
    /// @return amountOut The amount of the received token
    function exactInputSingle(ExactInputSingleParams calldata params) external payable returns (uint256 amountOut);

    struct ExactInputParams {
        bytes path;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
    }

    /// @notice Swaps amountIn of one token for as much as possible of another along the specified path
    /// @param params The multi-hop swap parameters
    /// @return amountOut The amount of the received token
    function exactInput(ExactInputParams calldata params) external payable returns (uint256 amountOut);
}

/// @title ICamelotRouter
/// @notice Interface for Camelot DEX Router on Arbitrum
interface ICamelotRouter {
    /// @notice Swap exact tokens for tokens via Camelot
    /// @param amountIn The amount of input tokens to swap
    /// @param amountOutMin The minimum amount of output tokens to receive
    /// @param path An array of token addresses representing the swap route
    /// @param to The recipient of the output tokens
    /// @param referrer The referrer address for fee sharing
    /// @param deadline The unix timestamp after which the transaction will revert
    function swapExactTokensForTokensSupportingFeeOnTransferTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        address[] calldata path,
        address to,
        address referrer,
        uint256 deadline
    ) external;

    /// @notice Get amounts out for a given input amount and path
    /// @param amountIn The input amount
    /// @param path An array of token addresses representing the swap route
    /// @return amounts The output amounts for each swap in the path
    function getAmountsOut(uint256 amountIn, address[] calldata path) external view returns (uint256[] memory amounts);
}

/// @title SwapHelper
/// @notice Library for executing token swaps on Arbitrum DEXes (Uniswap V3 and Camelot)
library SwapHelper {
    using SafeERC20 for IERC20;

    /// @dev Uniswap V3 SwapRouter on Arbitrum
    address internal constant UNISWAP_V3_ROUTER = 0xE592427A0AEce92De3Edee1F18E0157C05861564;

    /// @dev Camelot Router on Arbitrum
    address internal constant CAMELOT_ROUTER = 0xc873fEcbd354f5A56E00E710B90EF4201db2448d;

    /// @dev Common Uniswap V3 fee tiers
    uint24 internal constant FEE_LOW = 500; // 0.05%
    uint24 internal constant FEE_MEDIUM = 3000; // 0.30%
    uint24 internal constant FEE_HIGH = 10000; // 1.00%

    /// @notice DEX to use for the swap
    enum DEX {
        UniswapV3,
        Camelot
    }

    /// @notice Swap tokens using Uniswap V3 single-hop
    /// @param tokenIn The input token address
    /// @param tokenOut The output token address
    /// @param amountIn The input amount
    /// @param amountOutMin The minimum output amount (slippage protection)
    /// @param fee The Uniswap V3 pool fee tier
    /// @return amountOut The actual output amount received
    function swapUniswapV3Single(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 amountOutMin,
        uint24 fee
    ) internal returns (uint256 amountOut) {
        IERC20(tokenIn).forceApprove(UNISWAP_V3_ROUTER, amountIn);

        ISwapRouter.ExactInputSingleParams memory params = ISwapRouter.ExactInputSingleParams({
            tokenIn: tokenIn,
            tokenOut: tokenOut,
            fee: fee,
            recipient: address(this),
            deadline: block.timestamp,
            amountIn: amountIn,
            amountOutMinimum: amountOutMin,
            sqrtPriceLimitX96: 0
        });

        amountOut = ISwapRouter(UNISWAP_V3_ROUTER).exactInputSingle(params);
    }

    /// @notice Swap tokens using Uniswap V3 multi-hop
    /// @param path The encoded swap path (tokenIn, fee, intermediate..., fee, tokenOut)
    /// @param amountIn The input amount
    /// @param amountOutMin The minimum output amount (slippage protection)
    /// @return amountOut The actual output amount received
    function swapUniswapV3MultiHop(bytes memory path, uint256 amountIn, uint256 amountOutMin)
        internal
        returns (uint256 amountOut)
    {
        // Decode the first token from the path to approve the router
        address tokenIn;
        assembly {
            tokenIn := shr(96, mload(add(path, 32)))
        }
        IERC20(tokenIn).forceApprove(UNISWAP_V3_ROUTER, amountIn);

        ISwapRouter.ExactInputParams memory params = ISwapRouter.ExactInputParams({
            path: path,
            recipient: address(this),
            deadline: block.timestamp,
            amountIn: amountIn,
            amountOutMinimum: amountOutMin
        });

        amountOut = ISwapRouter(UNISWAP_V3_ROUTER).exactInput(params);
    }

    /// @notice Swap tokens using Camelot DEX
    /// @param tokenIn The input token address
    /// @param tokenOut The output token address
    /// @param amountIn The input amount
    /// @param amountOutMin The minimum output amount (slippage protection)
    /// @return amountOut The actual output amount received
    function swapCamelot(address tokenIn, address tokenOut, uint256 amountIn, uint256 amountOutMin)
        internal
        returns (uint256 amountOut)
    {
        IERC20(tokenIn).forceApprove(CAMELOT_ROUTER, amountIn);

        uint256 balanceBefore = IERC20(tokenOut).balanceOf(address(this));

        address[] memory path = new address[](2);
        path[0] = tokenIn;
        path[1] = tokenOut;

        ICamelotRouter(CAMELOT_ROUTER).swapExactTokensForTokensSupportingFeeOnTransferTokens(
            amountIn, amountOutMin, path, address(this), address(0), block.timestamp
        );

        amountOut = IERC20(tokenOut).balanceOf(address(this)) - balanceBefore;
    }

    /// @notice General-purpose swap function that routes to the best DEX
    /// @param dex The DEX to use
    /// @param tokenIn The input token address
    /// @param tokenOut The output token address
    /// @param amountIn The input amount
    /// @param amountOutMin The minimum output amount (slippage protection)
    /// @param fee The fee tier (only used for Uniswap V3; ignored for Camelot)
    /// @return amountOut The actual output amount received
    function swap(DEX dex, address tokenIn, address tokenOut, uint256 amountIn, uint256 amountOutMin, uint24 fee)
        internal
        returns (uint256 amountOut)
    {
        if (dex == DEX.UniswapV3) {
            amountOut = swapUniswapV3Single(tokenIn, tokenOut, amountIn, amountOutMin, fee);
        } else {
            amountOut = swapCamelot(tokenIn, tokenOut, amountIn, amountOutMin);
        }
    }
}

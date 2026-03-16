# CLAUDE.md

## 项目概述

Arbitrum L2 MEV bot，Rust + Solidity。两条业务线：借贷清算和 DEX 套利。零本金运行。

## 构建

```bash
cargo build --release
cd contracts && forge build && forge test && cd ..
```

## 测试

```bash
cargo test                    # Rust 16 tests
forge test                    # Solidity 50 tests (FlashLiquidator + FlashArbitrage)
```

## 运行

```bash
export LIQUIDATOR_PRIVATE_KEY=0x...
./target/release/arbitrum-liquidator --config config/prod.toml --dry-run
```

`--dry-run` 同时覆盖 `execution.dry_run` 和 `arbitrage.dry_run`。

## 关键架构

- **exec_provider 用 `Arc` 包装**：Liquidator 和 ArbitrageMonitor 必须共享同一个 NonceFiller 实例，`.clone()` 会创建独立 nonce 缓存导致冲突
- **Sequencer Feed 用 broadcast channel**：清算 monitors 和套利 monitor 各自订阅
- **revm 模拟**：simulator.rs（清算）和 mod.rs（套利）各自建 CacheDB + AlloyDB，模拟前预热合约字节码
- **FlashArbitrage 合约**：`executeMultiHop` 支持 2-8 池嵌套 flash swap，用 `_hops[step]` mapping 区分回调层级
- **pool_state.rs**：DashMap 缓存池状态，`refresh()` 用 Multicall 批量读 slot0

## 代码规范

- 不要在套利模块里用 `pool_a` / `pool_b`，统一用 `pools: Vec<Address>` + `zero_for_one: Vec<bool>`
- gas 费用计算统一用 `crate::utils::gas::arbitrum_gas_cost_usd()`，不要在各模块重复写
- 利润用 `AtomicI64`（支持负值），不要用 `AtomicU64`
- inflight 去重的 key 必须在所有退出路径（success/revert/timeout/send failure）清理
- 合约验证：启动时 `resolve_route` 从链上读 token0/token1 验证路由循环性，不依赖配置文件正确

## 配置

- `config/default.toml` 是模板，生产用 `config/prod.toml`
- `arbitrage.pairs` = 双池对（同 token pair），`arbitrage.routes` = 多跳路由（不同 token pair 环路）
- `arbitrage.routes` 只需指定 `pools[]` 和 `input_token`，代码自动计算 `zeroForOne`
- 清算协议全禁用时可以纯套利模式运行（不需要 flash_liquidator 合约）

## 合约

- `FlashLiquidator.sol`：已有 2 步 ownership 转移
- `FlashArbitrage.sol`：已有 2 步 ownership 转移、`InvalidAmount`/`InvalidPoolPair`/`InvalidRoute` 校验、step bounds 检查
- SushiSwap V3 和 UniV3 用相同的 `IUniswapV3Pool` + `uniswapV3SwapCallback` 接口，合约无需修改

## 已知限制

- Timeboost 快车道（200ms 优势）被 Selini/Wintermute 垄断，我们在普通车道竞争
- ETH 价格回退值硬编码 $3500（`CACHED_ETH_PRICE_CENTS` 未初始化时）
- `select_gas_price` 分层阈值（$10/$50）硬编码，未来可配置化
- Silo 协议未实现，启用会 bail

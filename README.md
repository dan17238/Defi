# Arbitrum Liquidation Bot

Arbitrum 链上 DeFi 清算机器人，覆盖 AAVE v3、Radiant Capital 协议，目标全链路延迟 <20ms。

## 架构

```
Sequencer Feed (0ms push)
    |
    v
健康因子扫描 (Multicall 批量查询)
    |
    v
revm 本地模拟 (~5ms)
    |
    v
签名 + 发送到 Sequencer (持久连接 ~13ms)
    |
    v
FlashLiquidator 合约 (原子执行)
    闪电贷 -> 清算 -> Swap -> 还贷 -> 利润
```

## 项目结构

```
├── src/                        # Rust bot
│   ├── main.rs                 # 入口，区块订阅，协议监控调度
│   ├── config.rs               # TOML 配置加载
│   ├── provider.rs             # RPC 连接 (WS/HTTP/签名)
│   ├── sequencer_feed.rs       # Sequencer Feed WebSocket 客户端
│   ├── web.rs                  # Dashboard API 服务
│   ├── protocols/
│   │   ├── aave_v3.rs          # AAVE v3 健康因子监控 + 借款人发现
│   │   ├── radiant.rs          # Radiant Capital 监控 + Chainlink 价格
│   │   └── silo.rs             # Silo Finance (骨架)
│   ├── liquidator/
│   │   ├── mod.rs              # 清算编排：模拟 -> 检查利润 -> 执行
│   │   ├── simulator.rs        # revm 本地模拟，计算利润
│   │   ├── executor.rs         # 签名 + 发送交易到 Sequencer
│   │   └── flash_loan.rs       # 闪电贷参数 ABI 编码 + Swap 路由
│   ├── state/
│   │   └── position_tracker.rs # DashMap 并发仓位追踪
│   └── utils/
│       ├── multicall.rs        # Multicall3 批量查询
│       └── metrics.rs          # 运行指标 (利润/延迟/成功率)
│
├── contracts/                  # Solidity 合约
│   ├── src/
│   │   ├── FlashLiquidator.sol # 主合约：闪电贷 + 清算 + Swap + 利润
│   │   ├── interfaces/         # AAVE v3, Radiant, Silo 接口
│   │   └── libraries/
│   │       └── SwapHelper.sol  # Uniswap V3 / Camelot DEX Swap
│   └── test/
│       └── FlashLiquidator.t.sol
│
├── dashboard/                  # 监控面板
│   ├── index.html              # Apple 风格前端 (实时延迟/利润/竞争对手)
│   ├── server.py               # Python 服务端探测 (延迟/链上数据)
│   └── chain_data.py           # 链上事件抓取 (清算/借款/竞争分析)
│
└── config/
    └── default.toml            # 配置文件
```

## 延迟

从 EC2 us-east-1 实测 (持久连接):

| 环节 | 延迟 |
|------|------|
| Sequencer Feed 推送 | 0ms (被动接收) |
| 状态读取 (本地节点) | <1ms |
| revm 模拟 | ~5ms |
| 发送到 Sequencer | ~13ms |
| **全链路** | **~19ms** |

## 安全特性

- 闪电贷零资金风险 — 不需要自有资金做清算
- `minProfit` 链上保护 — 合约层面拒绝亏本交易
- `minAmountOut` 滑点保护 — 防止三明治攻击
- 2 步所有权转移 — 防止误操作永久锁死合约
- revm 预模拟 — 链下验证利润后才发交易
- 重入保护 — `_inFlashLoan` 标志 + 转账后才重置
- Dashboard 绑定 127.0.0.1 — 仅通过 SSH 隧道访问

## 快速开始

### 环境要求

- Rust 1.80+
- Foundry (forge, cast, anvil)
- Docker (运行 Arbitrum Nitro 节点)

### 1. 编译

```bash
# Rust bot
cargo build --release

# Solidity 合约
cd contracts
forge install OpenZeppelin/openzeppelin-contracts
forge build
forge test
```

### 2. 配置

```bash
cp config/default.toml config/prod.toml
# 编辑 prod.toml:
# - 填入 RPC URL
# - 设置 dry_run = false (生产模式)

# 设置环境变量
export LIQUIDATOR_PRIVATE_KEY=你的私钥
```

### 3. 部署合约

```bash
cd contracts
forge create src/FlashLiquidator.sol:FlashLiquidator \
  --rpc-url https://arb1.arbitrum.io/rpc \
  --private-key $LIQUIDATOR_PRIVATE_KEY \
  --constructor-args 你的钱包地址

# 将返回的合约地址填入 config 的 contracts.flash_liquidator
```

### 4. 运行

```bash
# Dry-run 模式 (只模拟不发交易)
./target/release/arbitrum-liquidator --config config/prod.toml --dry-run

# 生产模式
./target/release/arbitrum-liquidator --config config/prod.toml
```

### 5. 监控面板

```bash
# 在 EC2 上启动 dashboard
cd dashboard && python3 server.py

# 本地通过 SSH 隧道访问
ssh -L 3000:127.0.0.1:3000 ubuntu@your-ec2-ip
# 浏览器打开 http://127.0.0.1:3000
```

## 协议支持

| 协议 | 状态 | 说明 |
|------|------|------|
| AAVE v3 | 完整 | 健康因子监控 + 闪电贷清算 |
| Radiant Capital | 完整 | AAVE v2 fork，复用逻辑 |
| Silo Finance | 骨架 | 接口已定义，Phase 4 实现 |

## 合约地址 (Arbitrum)

| 合约 | 地址 |
|------|------|
| AAVE v3 Pool | `0x794a61358D6845594F94dc1DB02A252b5b4814aD` |
| Radiant LendingPool | `0xF4B1486DD74D07706052A33d31d7c0AAFD0659E1` |
| Uniswap V3 Router | `0xE592427A0AEce92De3Edee1F18E0157C05861564` |
| Camelot Router | `0xc873fEcbd354f5A56E00E710B90EF4201db2448d` |
| Multicall3 | `0xcA11bde05977b3631167028862bE2a173976CA11` |
| Chainlink ETH/USD | `0x639Fe6ab55C921f74e7fac1ee960C0B6293ba612` |

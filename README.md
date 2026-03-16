# Arbitrum MEV Bot

Arbitrum L2 上的 DeFi MEV 机器人，覆盖**借贷清算**和 **DEX 套利**两条业务线。通过 Sequencer Feed 零延迟感知链上事件，revm 本地模拟验证盈利性，全链路目标 <5ms（清算）/ <3.5ms（套利）。

---

## 架构总览

```
Sequencer Feed (WebSocket, 被动推送)
    │
    ├─ broadcast ─┬──► 清算 Monitors (AAVE v3 / Radiant / Silo)
    │             │     └─ 借款人发现 → 健康因子扫描(Multicall3) → revm 模拟
    │             │         → FlashLiquidator 闪电贷原子清算
    │             │
    │             └──► 套利 Monitor
    │                   └─ Multicall 刷新池 slot0 → 价差检测
    │                       → revm 模拟 → FlashArbitrage 嵌套回调套利
    │
    └─ broadcast ──► Main Loop → 通知清算 Monitors 即时重扫
```

**两条业务线共享：** Sequencer Feed、revm 模拟器、Executor（签名+发送）、Metrics、Dashboard。

## 业务线

### 1. 借贷清算

监控 AAVE v3、Radiant、Silo 借贷协议，当 health factor < 1 时通过闪电贷执行零资金清算。

```
闪电贷(借债务 token) → liquidationCall(还债，拿抵押品)
    → Uniswap/Camelot Swap(抵押品→债务 token) → 还闪电贷 → 利润转 owner
```

### 2. DEX 套利

监控同 token pair 不同费率的 UniV3 池子（如 WETH/USDC 0.05% vs 0.3%），利用 flash swap 嵌套回调实现零成本套利。

```
poolA.swap(卖 token0，拿 token1)
    → 回调: poolB.swap(卖 token1，拿 token0)
        → 回调: 付 poolB 它要的 token1
    → 付 poolA 它要的 token0
    → 利润 = 从 poolB 拿到的 token0 - 欠 poolA 的 token0
```

**嵌套回调机制：** 合约用 `_arbPoolA` / `_arbPoolB` 存储地址区分两层回调：
- `msg.sender == _arbPoolA` → 第一层，执行 poolB 反向交易
- `msg.sender == _arbPoolB` → 第二层，付 poolB 欠款

---

## 一键部署

### 前提条件

- AWS EC2 实例（推荐 **us-east-2 Ohio**，离 Arbitrum Sequencer 最近）
- 推荐配置：8 核 / 32GB 内存 / 4TB+ 磁盘
- 一个有少量 Arbitrum ETH（0.01 ETH）的钱包

### Step 1: 安装依赖

```bash
sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev clang cmake docker.io
sudo usermod -aG docker $USER

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env

# Foundry
curl -L https://foundry.paradigm.xyz | bash
source ~/.bashrc
foundryup
```

### Step 2: 克隆 + 编译

```bash
git clone git@github.com:dan17238/Defi.git
cd Defi

# Rust bot
cargo build --release

# Solidity 合约
cd contracts
forge build && forge test
cd ..
```

### Step 3: 启动 Arbitrum 节点

```bash
sudo mkdir -p /data/arbitrum && sudo chown -R $USER:$USER /data

docker run -d \
  --name nitro-node \
  --restart unless-stopped \
  -v /data/arbitrum:/home/user/.arbitrum \
  -p 8547:8547 -p 8548:8548 \
  offchainlabs/nitro-node:v3.9.6-91bf578 \
  --parent-chain.connection.url="https://ethereum-rpc.publicnode.com" \
  --parent-chain.blob-client.beacon-url="https://ethereum-beacon-api.publicnode.com" \
  --chain.id=42161 \
  --init.latest=pruned \
  --http.api=net,web3,eth,debug \
  --http.corsdomain="*" --http.addr=0.0.0.0 --http.vhosts="*" \
  --ws.port=8548 --ws.addr=0.0.0.0

# 查看同步进度
docker logs --tail 5 nitro-node
du -sh /data/arbitrum/
```

### Step 4: 部署合约

```bash
export LIQUIDATOR_PRIVATE_KEY=你的私钥

# 清算合约
cd contracts
forge create src/FlashLiquidator.sol:FlashLiquidator \
  --rpc-url https://arb1.arbitrum.io/rpc \
  --private-key $LIQUIDATOR_PRIVATE_KEY
# 记下 Deployed to: 0x... (LIQUIDATOR_ADDR)

# 套利合约
forge create src/FlashArbitrage.sol:FlashArbitrage \
  --rpc-url https://arb1.arbitrum.io/rpc \
  --private-key $LIQUIDATOR_PRIVATE_KEY
# 记下 Deployed to: 0x... (ARBITRAGE_ADDR)
cd ..
```

### Step 5: 配置

```bash
cp config/default.toml config/prod.toml
```

编辑 `config/prod.toml`：

```toml
[rpc]
http_url = "http://127.0.0.1:8547"
ws_url = "ws://127.0.0.1:8548"
# 节点同步完后取消注释:
# ipc_path = "/data/arbitrum/arb1/nitro/ipc"

[contracts]
flash_liquidator = "LIQUIDATOR_ADDR"

[execution]
dry_run = true    # 先 dry-run 测试！
max_gas_price_gwei = 1.0
min_profit_usd = 0.5

[arbitrage]
enabled = true
dry_run = true    # 先 dry-run 测试！
min_profit_usd = 0.5
max_gas_price_gwei = 2.0
flash_arbitrage_contract = "ARBITRAGE_ADDR"
```

### Step 6: 运行

```bash
export LIQUIDATOR_PRIVATE_KEY=你的私钥

# 1) dry-run 测试
./target/release/arbitrum-liquidator --config config/prod.toml --dry-run

# 2) 确认日志正常后，正式运行
./target/release/arbitrum-liquidator --config config/prod.toml --no-dry-run

# 后台运行
nohup ./target/release/arbitrum-liquidator --config config/prod.toml > /tmp/bot.log 2>&1 &
```

### Step 7: 监控

```bash
# SSH 隧道访问 Dashboard
ssh -i key.pem -L 3000:127.0.0.1:3000 ubuntu@EC2-IP
# 浏览器 http://127.0.0.1:3000

# API
curl -s http://127.0.0.1:3000/api/metrics | python3 -m json.tool
```

---

## 延迟预算

### 清算链路

| 阶段 | 延迟 |
|------|------|
| Sequencer Feed 事件 | 0ms |
| 健康因子批量查询 (Multicall/IPC) | <0.5ms |
| revm 模拟 (预热缓存) | ~2ms |
| 签名 + 发送 | ~2.3ms |
| **全链路** | **~5ms** |

### 套利链路

| 阶段 | 延迟 |
|------|------|
| Sequencer Feed 事件 | 0ms |
| 过滤 + Multicall 刷新 slot0 (IPC) | <0.3ms |
| 价差检测 + 最优量计算 | <0.1ms |
| revm 模拟 (预热缓存) | ~1ms |
| 签名 + 发送 | ~0.8ms |
| **全链路** | **~2.3ms** |

## 安全特性

- **零资金风险** — 闪电贷（清算）和 flash swap（套利）均无需自有资金
- **链上利润保护** — 两个合约均有 `minProfit` 参数，亏本交易自动 revert
- **revm 预模拟** — 本地完整模拟确认盈利后才发交易
- **动态 Gas** — 套利按利润分级定价（>$50→1gwei, >$10→0.1, 其他→0.02）
- **重入保护** — 清算合约 `_inFlashLoan` + 套利合约 `_executing` + 池地址白名单
- **启动校验** — pairs 配置的 token0/token1 在初始化时做链上比对，不匹配直接 bail
- **2 步所有权** — 防止误操作永久锁死合约资金
- **Dashboard 仅本地** — 绑定 127.0.0.1，须 SSH 隧道访问

## 监控池对 (套利)

| 池对 | 费率 | 说明 |
|------|------|------|
| WETH/USDC 0.05% <> 0.3% | 0.35% | 主力，流动性最大 |
| WBTC/WETH 0.05% <> 0.3% | 0.35% | 大额机会 |

## 项目结构

```
├── src/
│   ├── main.rs                   # 入口: broadcast channel, 各模块 spawn
│   ├── config.rs                 # TOML 配置 (含 ArbitrageConfig)
│   ├── provider.rs               # RPC 连接 (IPC/WS/HTTP/签名)
│   ├── sequencer_feed.rs         # Sequencer Feed → broadcast channel
│   ├── web.rs                    # Dashboard API (Axum)
│   │
│   ├── arbitrage/                # === DEX 套利模块 ===
│   │   ├── mod.rs                # ArbitrageMonitor: feed→检测→模拟→执行
│   │   ├── detector.rs           # sqrtPriceX96 价差检测 + 最优量计算
│   │   ├── pool_state.rs         # DashMap 池状态缓存 (Multicall batch)
│   │   └── pairs.rs              # 池对配置解析 + token 校验
│   │
│   ├── liquidator/               # === 借贷清算模块 ===
│   │   ├── mod.rs                # 编排: 模拟→利润检查→执行
│   │   ├── simulator.rs          # revm 模拟 + 缓存预热
│   │   ├── executor.rs           # 签名 + 发送到 Sequencer
│   │   └── flash_loan.rs         # ABI 编码 + token 定价 + Swap 路由
│   │
│   ├── protocols/                # 借贷协议适配
│   │   ├── aave_v3.rs            # AAVE v3 借款人发现 + 健康因子
│   │   ├── radiant.rs            # Radiant (含 Chainlink ETH 价格缓存)
│   │   └── silo.rs               # Silo 骨架
│   │
│   ├── state/
│   │   └── position_tracker.rs   # DashMap 并发仓位追踪
│   └── utils/
│       ├── multicall.rs          # Multicall3 批量 RPC
│       └── metrics.rs            # 运行指标 (清算+套利+延迟)
│
├── contracts/
│   ├── src/
│   │   ├── FlashLiquidator.sol   # 清算合约 (闪电贷+liquidation+Swap)
│   │   ├── FlashArbitrage.sol    # 套利合约 (嵌套 flash swap 回调)
│   │   ├── interfaces/
│   │   │   ├── IAaveV3Pool.sol
│   │   │   ├── IRadiantPool.sol
│   │   │   ├── ISilo.sol
│   │   │   └── IUniswapV3Pool.sol
│   │   └── libraries/
│   │       └── SwapHelper.sol    # Uniswap V3 / Camelot Swap
│   └── test/
│       ├── FlashLiquidator.t.sol # 23 个测试
│       └── FlashArbitrage.t.sol  # 14 个测试
│
├── dashboard/                    # 监控面板
└── config/
    └── default.toml              # 配置模板
```

## 运维

```bash
# 查看节点同步
docker logs --tail 5 nitro-node
du -sh /data/arbitrum/

# Bot 日志
tail -f /tmp/bot.log

# 更新代码
cd ~/Defi && git pull && cargo build --release

# 重启节点
docker restart nitro-node
```

## 合约地址 (Arbitrum One)

| 合约 | 地址 |
|------|------|
| AAVE v3 Pool | `0x794a61358D6845594F94dc1DB02A252b5b4814aD` |
| AAVE v3 DataProvider | `0x69FA688f1Dc47d4B5d8029D5a35FB7a548310654` |
| Uniswap V3 Router | `0xE592427A0AEce92De3Edee1F18E0157C05861564` |
| Camelot Router | `0xc873fEcbd354f5A56E00E710B90EF4201db2448d` |
| Multicall3 | `0xcA11bde05977b3631167028862bE2a173976CA11` |
| Chainlink ETH/USD | `0x639Fe6ab55C921f74e7fac1ee960C0B6293ba612` |
| Arbitrum Sequencer | `arb1-sequencer.arbitrum.io` (Ohio, us-east-2) |

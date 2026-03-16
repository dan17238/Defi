# Arbitrum Liquidation Bot

Arbitrum 链上 DeFi 清算机器人，目标全链路延迟 ~5ms。通过闪电贷执行零资金清算，利润自动转入钱包。

---

## 一键部署指南

在一台全新的 Ubuntu EC2 上，从零到上线只需以下步骤。

### 前提条件

- AWS EC2 实例（推荐 **us-east-2 Ohio**，离 Arbitrum Sequencer 最近）
- 推荐配置：8 核 / 32GB 内存 / 2TB+ 磁盘
- 一个有少量 Arbitrum ETH（0.01 ETH）的钱包

### Step 1: 安装依赖（约 5 分钟）

```bash
# 系统依赖
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

### Step 2: 克隆代码 + 编译（约 5 分钟）

```bash
# SSH key（需要加到 GitHub: https://github.com/settings/keys）
ssh-keygen -t ed25519 -N "" -f ~/.ssh/id_ed25519
cat ~/.ssh/id_ed25519.pub
ssh-keyscan github.com >> ~/.ssh/known_hosts

# 克隆
git clone git@github.com:dan17238/Defi.git
cd Defi

# 编译 Rust bot
cargo build --release

# 编译合约
cd contracts
git init && git add -A && git commit -m "init"
forge install OpenZeppelin/openzeppelin-contracts
forge build && forge test
cd ..
```

### Step 3: 启动 Arbitrum 节点（约 2-4 小时同步）

```bash
sudo mkdir -p /data/arbitrum
sudo chown -R $USER:$USER /data

sudo docker run -d \
  --name nitro-node \
  --restart unless-stopped \
  -v /data/arbitrum:/home/user/.arbitrum \
  -p 0.0.0.0:8547:8547 \
  -p 0.0.0.0:8548:8548 \
  offchainlabs/nitro-node:v3.9.6-91bf578 \
  --parent-chain.connection.url="https://ethereum-rpc.publicnode.com" \
  --parent-chain.blob-client.beacon-url="https://ethereum-beacon-api.publicnode.com" \
  --chain.id=42161 \
  --http.api=net,web3,eth,debug \
  --http.corsdomain="*" \
  --http.addr=0.0.0.0 \
  --http.vhosts="*" \
  --ws.port=8548 \
  --ws.addr=0.0.0.0 \
  --init.latest=pruned

# 查看同步进度
sudo docker logs --tail 1 nitro-node 2>&1 | grep -oP "\d+\.\d+%"
```

### Step 4: 部署清算合约

```bash
# 设置私钥
export LIQUIDATOR_PRIVATE_KEY=你的私钥

# 部署（需要钱包有 ~0.005 ETH gas）
cd contracts
forge create src/FlashLiquidator.sol:FlashLiquidator \
  --rpc-url https://arb1.arbitrum.io/rpc \
  --private-key $LIQUIDATOR_PRIVATE_KEY

# 记下返回的 Deployed to: 0x... 地址
```

### Step 5: 配置

```bash
cd ~/Defi
cp config/default.toml config/prod.toml
```

编辑 `config/prod.toml`：

```toml
[rpc]
http_url = "http://127.0.0.1:8547"
ws_url = "ws://127.0.0.1:8548"
# 节点同步完后取消注释:
# ipc_path = "/data/arbitrum/arb1/nitro/ipc"

[wallet]
private_key_env = "LIQUIDATOR_PRIVATE_KEY"

[contracts]
flash_liquidator = "0x你部署的合约地址"

[protocols.aave_v3]
enabled = true
pool = "0x794a61358D6845594F94dc1DB02A252b5b4814aD"
data_provider = "0x69FA688f1Dc47d4B5d8029D5a35FB7a548310654"
min_profit_usd = 1.0

[protocols.radiant]
enabled = false  # Radiant 已停止运营

[protocols.silo]
enabled = false

[execution]
dry_run = true   # 先用 dry-run 测试！
max_gas_price_gwei = 1.0
min_profit_usd = 0.5
multicall_batch_size = 100

[sequencer]
feed_url = "wss://arb1.arbitrum.io/feed"
rpc_url = "https://arb1-sequencer.arbitrum.io/rpc"

[monitoring]
metrics_port = 9090
dashboard_port = 3000
log_level = "info"
```

### Step 6: 运行

```bash
# 设置私钥环境变量
export LIQUIDATOR_PRIVATE_KEY=你的私钥

# 1) 先 dry-run 测试（只模拟，不发交易）
./target/release/arbitrum-liquidator --config config/prod.toml --dry-run

# 2) 确认没问题后，正式运行
# 修改 config: dry_run = false
./target/release/arbitrum-liquidator --config config/prod.toml

# 后台运行
nohup ./target/release/arbitrum-liquidator --config config/prod.toml > /tmp/bot.log 2>&1 &
```

### Step 7: 监控面板

```bash
# 启动 dashboard（在 EC2 上）
cd ~/Defi/dashboard
nohup python3 server.py > /tmp/dashboard.log 2>&1 &

# 从本地电脑通过 SSH 隧道访问
ssh -i your-key.pem -L 3000:127.0.0.1:3000 ubuntu@你的EC2-IP
# 浏览器打开 http://127.0.0.1:3000
```

---

## 架构

```
Sequencer Feed (0ms 被动推送)
    |
    v
借款人发现 (扫描 Borrow 事件) + 健康因子批量查询 (Multicall3)
    |
    v
revm 本地模拟 (~2ms，缓存预热)
    |
    v
签名 + 持久连接发送到 Sequencer (~2.3ms from Ohio)
    |
    v
FlashLiquidator.sol 链上原子执行:
    AAVE v3 闪电贷 -> liquidationCall -> Uniswap Swap -> 还贷 -> 利润转 owner
```

## 延迟实测

| 环节 | us-east-1 Virginia | us-east-2 Ohio |
|------|-------------------|---------------|
| Sequencer Feed | 0ms | 0ms |
| 状态读取 (本地 IPC) | <0.3ms | <0.3ms |
| revm 模拟 (预热) | ~2ms | ~2ms |
| 签名 | <0.5ms | <0.5ms |
| 发送到 Sequencer | 13ms | **2.3ms** |
| **全链路** | **~16ms** | **~5ms** |

**推荐 us-east-2 (Ohio)** — Arbitrum Sequencer 在 Ohio (AWS us-east-2)，同 region 延迟最低。

## 安全特性

- **零资金风险** — 全部使用闪电贷，不需要自有清算资金
- **链上利润保护** — `minProfit` 参数，合约层面拒绝亏本交易
- **滑点保护** — `minAmountOut` 参数（2% 最大滑点），防止三明治攻击
- **revm 预模拟** — 链下模拟验证利润后才发交易
- **2 步所有权转移** — 防止误操作永久锁死合约资金
- **重入保护** — `_inFlashLoan` 标志，所有转账完成后才重置
- **审批清零** — 每次清算后重置 token 审批，防止悬空授权
- **Dashboard 仅本地** — 绑定 127.0.0.1，必须通过 SSH 隧道访问

## 项目结构

```
├── src/                          # Rust bot
│   ├── main.rs                   # 入口: 启动各模块, 事件循环
│   ├── config.rs                 # TOML 配置加载 + 校验
│   ├── provider.rs               # RPC 连接 (IPC/WS/HTTP/签名)
│   ├── sequencer_feed.rs         # Sequencer Feed WebSocket 订阅
│   ├── web.rs                    # Dashboard API (Axum)
│   ├── protocols/
│   │   ├── mod.rs                # Protocol trait 定义
│   │   ├── aave_v3.rs            # AAVE v3: 借款人发现 + 健康因子扫描
│   │   ├── radiant.rs            # Radiant: 已停运, 保留代码
│   │   └── silo.rs               # Silo: 骨架 (Phase 4)
│   ├── liquidator/
│   │   ├── mod.rs                # 编排: 模拟 -> 利润检查 -> 执行
│   │   ├── simulator.rs          # revm 模拟 + 缓存预热
│   │   ├── executor.rs           # 签名 + 发送到 Sequencer
│   │   └── flash_loan.rs         # ABI 编码 + Swap 路由选择
│   ├── state/
│   │   └── position_tracker.rs   # DashMap 并发仓位追踪
│   └── utils/
│       ├── multicall.rs          # Multicall3 批量 RPC
│       └── metrics.rs            # 运行指标 (利润/延迟/成功率)
│
├── contracts/                    # Solidity
│   ├── src/
│   │   ├── FlashLiquidator.sol   # 主合约 (闪电贷+清算+Swap)
│   │   ├── interfaces/           # AAVE v3, Radiant, Silo 接口
│   │   └── libraries/
│   │       └── SwapHelper.sol    # Uniswap V3 / Camelot Swap
│   └── test/
│       └── FlashLiquidator.t.sol # 23 个测试
│
├── dashboard/                    # 监控面板
│   ├── index.html                # 前端 (延迟/利润/竞争对手/清算)
│   ├── server.py                 # Python 服务端延迟探测
│   └── chain_data.py             # 链上事件抓取
│
└── config/
    └── default.toml              # 默认配置模板
```

## 运维

### 查看节点同步进度
```bash
sudo docker logs --tail 1 nitro-node 2>&1 | grep -oP "\d+\.\d+%"
du -sh /data/arbitrum/
```

### 查看 bot 日志
```bash
tail -f /tmp/bot.log
```

### 更新代码
```bash
cd ~/Defi
git pull
cargo build --release
# 重启 bot
```

### 重启节点
```bash
sudo docker restart nitro-node
```

### 查看实时延迟
```bash
curl -s http://127.0.0.1:3000/api/latency | python3 -m json.tool
```

## 竞争情况 (2026-03-16 实测)

| 指标 | 数据 |
|------|------|
| AAVE v3 月清算量 | ~392 笔 |
| 活跃竞争 bot | 7 个 |
| 平均每笔利润 | $1-750 (多数 <$50) |
| 我们延迟 | 5ms (最快) |
| Radiant | 已停运，0 清算 |
| Dolomite | 215笔/月，但 36 个 bot 竞争 |

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

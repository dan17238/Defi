# Arbitrum MEV Bot

Arbitrum L2 上的高频 MEV 机器人。两条业务线：**借贷清算**（AAVE v3 / Radiant）和 **DEX 套利**（UniV3 / SushiSwap 跨池 + 多跳环形路由）。零本金运行（闪电贷 + flash swap），全链路 <3ms。

## 架构

```
Sequencer Feed (WebSocket)
    │
    ├─ broadcast ─┬─► 清算 Monitors (AAVE v3 / Radiant)
    │             │    健康因子扫描 → revm 模拟 → FlashLiquidator 原子清算
    │             │
    │             ├─► 套利 Monitor
    │             │    tx_to 过滤 → Multicall 刷新 slot0 → 价差检测
    │             │    → bracket search 最优量 → revm 模拟 → FlashArbitrage 执行
    │             │
    │             └─► Main Loop → 区块订阅 + WS 重连
    │
    └─ 共享 exec_provider (Arc) ← NonceFiller 单实例
```

## 业务线

### 清算
- 协议：AAVE v3、Radiant（Silo 预留）
- 机制：闪电贷借债务 token → `liquidationCall` → Swap 抵押品 → 还贷 → 利润
- 零本金，失败仅损 gas (~$0.04)
- receipt 异步跟踪，解析 `LiquidationExecuted` 事件获取真实利润

### DEX 套利
- **双池套利**：同 token pair 不同费率 / 跨 DEX（UniV3 ↔ SushiSwap V3）
- **多跳路由**：2-8 池嵌套 flash swap 环形套利（如 WETH→USDC→USDC.e→WETH）
- 16 个双池对 + 3 条三角路由，覆盖 WETH/USDC/USDT/ARB/WBTC/LINK/DAI
- 跨 DEX 同费率对门槛最低（15 bps），每天 ~441 次机会（WETH/USDC Uni↔Sushi）
- bracket search（0.5x/1x/2x/4x）找最优交易量
- Semaphore(3) 限制并发模拟

## 延迟

| 阶段 | 清算 | 套利 |
|------|------|------|
| Feed 事件 | 0ms | 0ms |
| tx_to 过滤 | - | <0.01ms |
| Multicall/IPC | <0.5ms | <0.3ms |
| revm 模拟 | ~2ms | ~1ms (×4 bracket) |
| 签名 + 发送 | ~0.8ms | ~0.8ms |
| **全链路** | **~3.3ms** | **~2.6ms** |

## 部署

### 1. 安装

```bash
sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev clang cmake docker.io
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
curl -L https://foundry.paradigm.xyz | bash && foundryup
```

### 2. 编译

```bash
git clone git@github.com:dan17238/Defi.git && cd Defi
cargo build --release
cd contracts && forge build && forge test && cd ..
```

### 3. 节点

```bash
sudo mkdir -p /data/arbitrum && sudo chown -R $USER:$USER /data
# 用 aria2 下载快照（支持断点续传）
sudo apt install -y aria2
for i in 0000 0001 0002 0003 0004; do
  aria2c -c -x16 -s16 -k20M --file-allocation=none \
    -d /data/snapshot-parts \
    "https://snapshot.arbitrum.foundation/arb1/$(curl -s https://snapshot.arbitrum.foundation/arb1/latest)/pruned.tar.part${i}" &
done
wait
# 合并并启动
cat /data/snapshot-parts/pruned.tar.part* | tar -xf - -C /data/arbitrum/
docker run -d --name nitro-node --restart unless-stopped \
  -v /data/arbitrum:/home/user/.arbitrum \
  -p 8547:8547 -p 8548:8548 \
  offchainlabs/nitro-node:v3.9.6-91bf578 \
  --parent-chain.connection.url="https://ethereum-rpc.publicnode.com" \
  --parent-chain.blob-client.beacon-url="https://ethereum-beacon-api.publicnode.com" \
  --chain.id=42161 \
  --http.api=net,web3,eth,debug --http.addr=0.0.0.0 --http.vhosts="*" \
  --ws.port=8548 --ws.addr=0.0.0.0
```

### 4. 部署合约

```bash
export LIQUIDATOR_PRIVATE_KEY=你的私钥
cd contracts

# 清算合约
forge create src/FlashLiquidator.sol:FlashLiquidator \
  --rpc-url https://arb1.arbitrum.io/rpc --private-key $LIQUIDATOR_PRIVATE_KEY

# 套利合约
forge create src/FlashArbitrage.sol:FlashArbitrage \
  --rpc-url https://arb1.arbitrum.io/rpc --private-key $LIQUIDATOR_PRIVATE_KEY
```

### 5. 配置

```bash
cp config/default.toml config/prod.toml
# 编辑 prod.toml：填入合约地址、切换到本地节点 RPC
```

### 6. 运行

```bash
export LIQUIDATOR_PRIVATE_KEY=你的私钥
# dry-run
./target/release/arbitrum-liquidator --config config/prod.toml --dry-run
# 上线
./target/release/arbitrum-liquidator --config config/prod.toml --no-dry-run
```

## 安全

- 零本金：闪电贷（清算）+ flash swap（套利）
- 链上 `minProfit` 保护，亏本交易自动 revert
- revm 预模拟 + bracket search 验证盈利性
- 2 步 ownership 转移（FlashLiquidator + FlashArbitrage）
- inflight DashSet 去重，防止重复提交
- 45 秒 receipt 超时，防止 spawned task 泄漏
- 共享 `Arc<exec_provider>` 避免 nonce 冲突

## 项目结构

```
src/
├── main.rs                    # 入口：条件初始化、broadcast channel、WS 重连
├── config.rs                  # TOML 配置 + 校验 (含 ArbitrageRouteConfig)
├── provider.rs                # IPC/WS/HTTP/签名 provider
├── sequencer_feed.rs          # Feed 订阅 + RLP 解码 tx_to
├── web.rs                     # Dashboard API (/api/metrics, /api/arb)
│
├── arbitrage/                 # DEX 套利
│   ├── mod.rs                 # ArbitrageMonitor: feed→过滤→检测→bracket→模拟→执行
│   ├── detector.rs            # 双池价差 + 多跳环路利润检测
│   ├── pool_state.rs          # DashMap 池缓存 (Multicall batch)
│   ├── pairs.rs               # 池对配置解析 + token 校验
│   └── dashboard.rs           # 实时事件/池状态/价差历史
│
├── liquidator/                # 借贷清算
│   ├── mod.rs                 # 编排：buffer_unordered(3) 并发模拟
│   ├── simulator.rs           # revm 模拟 + 缓存预热
│   ├── executor.rs            # 签名发送 + receipt 跟踪 + 真实利润解析
│   └── flash_loan.rs          # ABI 编码 + token 定价 + Swap 路由
│
├── protocols/                 # 借贷协议
│   ├── aave_v3.rs             # AAVE v3 借款人发现 + 健康因子
│   └── radiant.rs             # Radiant (含 Chainlink ETH 价格缓存)
│
├── utils/
│   ├── gas.rs                 # 统一 gas 费用计算 (含 L1 data fee)
│   ├── metrics.rs             # AtomicI64 利润计数 + 清算/套利分离
│   └── multicall.rs           # Multicall3 批量 RPC
│
contracts/
├── FlashLiquidator.sol        # 闪电贷清算 (AAVE v3 / Radiant)
├── FlashArbitrage.sol         # 多跳 flash swap (2-8 池, executeMultiHop)
├── interfaces/                # AAVE, Radiant, UniV3 接口
└── libraries/SwapHelper.sol   # UniV3 / Camelot swap

dashboard/
├── index.html                 # Canvas 拓扑图 + 气泡仓位图 + Claude 配色
├── server.py                  # 延迟探测服务
└── chain_data.py              # 多协议链上数据 (AAVE + Radiant)
```

## 监控池对

| 类别 | 对数 | 门槛 | 说明 |
|------|------|------|------|
| 同 DEX 跨费率 | 11 | 11-40 bps | UniV3 不同 fee tier |
| 跨 DEX 同费率 | 5 | 15 bps | UniV3 ↔ SushiSwap V3 |
| 三角路由 | 3 | 36 bps | WETH→USDC→USDC.e→WETH 等 |

## 运维

```bash
# 节点状态
docker logs --tail 5 nitro-node

# Bot 日志
tail -f /tmp/bot.log

# 更新
cd ~/Defi && git pull && cargo build --release

# Dashboard (SSH 隧道)
ssh -L 3000:127.0.0.1:3000 ubuntu@EC2-IP
```

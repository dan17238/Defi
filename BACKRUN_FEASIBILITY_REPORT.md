# Arbitrum MEV Backrunning 可行性研究报告

**日期:** 2026-03-16
**项目:** arbitrum-liquidator backrun 模块可行性评估

---

## 目录

1. [Arbitrum Sequencer 排序机制](#1-arbitrum-sequencer-排序机制)
2. [Arbitrum MEV 生态现状 (2025-2026)](#2-arbitrum-mev-生态现状-2025-2026)
3. [Backrunning 在 Arbitrum 上的可行性](#3-backrunning-在-arbitrum-上的可行性)
4. [Sequencer Feed 技术细节](#4-sequencer-feed-技术细节)
5. [Timeboost 优先排序机制](#5-timeboost-优先排序机制)
6. [与我们现有设施的适配分析](#6-与我们现有设施的适配分析)
7. [收入估算](#7-收入估算)
8. [风险分析](#8-风险分析)
9. [结论与建议](#9-结论与建议)

---

## 1. Arbitrum Sequencer 排序机制

### 1.1 核心排序策略：FCFS + Timeboost

Arbitrum 的交易排序经历了两个阶段：

**第一阶段（至 2025 年 4 月）：纯 FCFS**
- Sequencer 严格按接收顺序排序交易
- 不存在 mempool，交易直接发到 Sequencer
- 无法通过提高 gas price 获得优先权
- 优点：简单、低延迟（250ms 出块）、天然防止三明治攻击
- 缺点：引发"延迟竞赛"——搜索者花重金部署低延迟基础设施

**第二阶段（2025 年 4 月至今）：Timeboost（修改版 FCFS）**
- 在 FCFS 基础上引入"快车道"（Express Lane）机制
- 非快车道交易被人为延迟 **200ms**
- 快车道控制者的交易 **零延迟** 被排序
- 每 60 秒一轮拍卖决定快车道控制者

### 1.2 Mempool 状态

**Arbitrum 不存在公开 mempool。** 交易直接提交给中心化 Sequencer，其他用户/节点无法在交易被排序前看到它。这从架构上杜绝了传统意义的抢跑（frontrunning）和三明治攻击。

### 1.3 延迟收件箱（Delayed Inbox）

- 用户可通过 L1 以太坊的 DelayedInbox 合约绕过 Sequencer 提交交易
- Sequencer 通常会自动拾取处理
- 如 24 小时内未处理，可调用 `forceInclude` 强制包含
- 提供抗审查保证，但延迟太大，不适合 MEV 场景

### 1.4 关键结论

> **Gas price 在 Arbitrum 上不决定排序，延迟才是一切。** 在 Timeboost 机制下，要么赢得快车道拍卖获得 200ms 优势，要么在普通车道与所有人共享 200ms 延迟。

---

## 2. Arbitrum MEV 生态现状 (2025-2026)

### 2.1 MEV 活动概况

根据学术研究 "Rolling in the Shadows"（CCS 2024）对 2021-2023 年的分析：

- **套利普遍存在：** 2023 年 4 月 Arbitrum 的套利交易数量是以太坊的 7.2 倍
- **无三明治攻击：** 在所有 Rollup 上未检测到三明治交易
- **利润显著低于以太坊：** 虽然交易量可比，但 Rollup 上的利润远低于以太坊主网
- **超 50 万个未被利用的套利机会** 持续存在 10-20 个区块

### 2.2 主要玩家（Timeboost 时代）

根据 2025 年 4-7 月的 151,423 次拍卖数据：

| 玩家 | 拍卖胜率 | 快车道交易占比 | 策略 |
|------|----------|----------------|------|
| **Selini Capital** | **59.92%** | 41.99% | CEX-DEX 套利，低价竞标 |
| **Wintermute** | **30%+** | 57.36% | CEX-DEX 套利，交易量最大 |
| **Kairos** | ~9% | <1% | 早期激进竞标，后趋于保守 |
| 其他 13 个地址 | <1% | 微量 | 偶发参与 |

**关键发现：**
- Selini + Wintermute 赢得 **>90%** 的拍卖
- 3 个实体控制 **>99%** 的轮次
- **94% 的快车道交易** 用于 CEX-DEX 套利
- **~22% 的快车道交易 revert**（表明 Timeboost 未能有效减少垃圾交易）
- 拍卖竞争随时间下降，DAO 收入持续减少

### 2.3 MEV 市场规模

| 指标 | 数值 |
|------|------|
| Timeboost 累计收入（至 2026 年 2 月） | **$6.74M**（约 1,500 ETH） |
| 月均 Timeboost 收入 | **$600K-700K**（趋势下降） |
| Arbitrum DAO 总收入占比 | Timeboost 占 ~50% |
| Arbitrum TVL | ~$2.1B（截至 2026 年 3 月） |
| Arbitrum DEX 日交易量 | 数亿美元级别 |

### 2.4 MEV 保护机制

- 无公开 mempool → 防止三明治攻击
- Timeboost 快车道控制者无法查看/重排他人交易
- 用户交易在排序前保持私密
- FCFS + 200ms 延迟限制了延迟竞赛的边际收益

---

## 3. Backrunning 在 Arbitrum 上的可行性

### 3.1 Backrunning 的核心挑战

与以太坊上 Flashbots/MEV-Share 提供的原子化 backrun bundle 不同，Arbitrum 上的 backrunning 面临根本性不同：

**以太坊 Backrunning（对比参考）:**
```
用户大额 Swap 出现在 mempool → 搜索者构建 backrun bundle
→ 通过 Flashbots 提交 → bundle 中用户tx + backrun tx 原子打包到同一区块
```

**Arbitrum Backrunning（实际情况）:**
```
用户交易被 Sequencer 接收 → 立即排序（无 mempool 可见）
→ Sequencer Feed 广播已排序的交易 → 搜索者才看到 → 已经太晚
→ 只能寄希望于价格不等式在下一个区块仍然存在
```

### 3.2 两种 Backrunning 模式

#### 模式 A：通过 Timeboost 快车道 Backrun

- **机制：** 赢得拍卖 → 获得 200ms 时间优势 → 在普通交易被延迟 200ms 期间插入自己的 backrun 交易
- **优势：** 200ms 窗口相对充足
- **致命问题：**
  - 你同样看不到其他人的 pending 交易（mempool 私密）
  - 你只能 backrun **已排序**的交易
  - 拍卖成本高：尽管目前竞争减少，底价已从 0.001 ETH 提高到 0.0075 ETH
  - 需要与 Selini/Wintermute 这样的顶级做市商竞争
  - **他们有 CEX 实时价格数据** 作为信息优势，这是纯 DEX 搜索者无法比拟的

#### 模式 B：不买快车道，依赖延迟优势

- **机制：** 从 Sequencer Feed 检测到大额 Swap → 快速模拟 → 发送 backrun 交易（受 200ms 延迟）
- **优势：** 零拍卖成本
- **致命问题：**
  - 200ms 延迟意味着你的交易至少比快车道控制者慢 200ms
  - 在快车道存在的情况下，任何有价值的 backrun 机会都会被快车道控制者抢先
  - 你与所有其他非快车道搜索者竞争

### 3.3 现有搜索者如何 Backrun

根据研究数据，当前 Arbitrum 上成功的 backrunning 实际上是 **CEX-DEX 套利**：

1. **信息来源：** CEX 实时价格（Binance/OKX websocket，延迟 <1ms）
2. **检测方式：** 比较 CEX 价格 vs DEX 池状态，不需要看到用户交易
3. **执行方式：** 通过 Timeboost 快车道提交套利交易
4. **本质：** 这不是"backrun 某个用户交易"，而是"利用 CEX-DEX 价差进行套利"

> 真正意义上的 "backrun 大额 Swap" 在 Arbitrum 上几乎不可行，因为你无法在交易排序前看到它。

### 3.4 可行的替代策略

如果不买快车道，存在以下机会：

1. **跨区块套利：** 大额 Swap 造成的价格不等式可能持续多个区块（研究显示平均 10-20 个区块未被利用），在这些不等式消失前执行套利
2. **清算 + 套利联动（我们已有）：** 借贷协议清算产生的价格冲击 → 跨池套利
3. **长尾资产套利：** Selini/Wintermute 主要关注主流交易对，长尾资产竞争较少

---

## 4. Sequencer Feed 技术细节

### 4.1 Feed 数据结构

```
BroadcastFeedMessage {
    SequenceNumber: u64,
    Message: MessageWithMetadata {
        Message: L1IncomingMessage {
            Header: L1IncomingMessageHeader {
                Kind: u8,
                Poster: Address,
                BlockNumber: u64,
                Timestamp: u64,
                RequestId: Option<H256>,
                L1BaseFee: U256,
            },
            L2msg: Vec<u8>,  // 核心：已编码的 L2 交易数据
        },
        DelayedMessagesRead: u64,
    },
    Signature: Option<Signature>,
    BlockMetadata: Option<BlockMetadata>,
}
```

### 4.2 关键问题：能看到 Pending 交易吗？

**不能。** Sequencer Feed 广播的是 **已排序的交易**，不是 pending 交易。

时间线如下：
```
t=0ms    用户发送交易到 Sequencer
t≈0.1ms  Sequencer 排序交易（分配 sequence number）
t≈0.5ms  Sequencer Feed 广播排序结果
t≈250ms  交易被包含到区块中
```

你从 Feed 看到交易时，它已经被排序了。你的 backrun 交易只能进入下一个排序位置。

### 4.3 Feed 连接方式

- **WebSocket 端口：** 9642（本地 relay）或直连 Sequencer Feed
- **Arbitrum One Feed URL：** `wss://arb1.sequencer.nitro.arbitrum.io/feed`
- **数据格式：** JSON-encoded BroadcastFeedMessage
- **解码方法：** `ParseL2Transactions()` 函数解码 L2msg 字段

### 4.4 Rust 实现方案

| 库 | 特点 | 局限 |
|----|------|------|
| `sequencer-client-rs` | 纯 Rust，MEV 特定功能 | 不完全支持批量交易解码 |
| `sequencer_client` (nuntax) | 支持所有交易类型包括批量交易（占 80%）| 功能更全面 |

### 4.5 延迟分析

| 路径 | 延迟 |
|------|------|
| Sequencer Feed 事件到达 | ~0.5-1ms（取决于地理位置） |
| 解码交易 | ~0.1ms |
| revm 模拟 | ~1-2ms |
| 签名 + 发送到 Sequencer | ~0.8-2ms |
| **总计** | **~2.5-5ms** |

**关键约束：** 即使你做到 2.5ms 全链路，交易仍然受 200ms Timeboost 延迟（除非你是快车道控制者）。

---

## 5. Timeboost 优先排序机制

### 5.1 机制详解

```
每 60 秒一轮
    │
    ├─ 拍卖阶段（轮开始前 15 秒）
    │   ├─ 密封投标（第二价格拍卖 / Vickrey 拍卖）
    │   ├─ 出价以 ETH 计价
    │   └─ 底价：0.0075 ETH（2025 年底上调）
    │
    ├─ 执行阶段（60 秒）
    │   ├─ 胜者获得快车道控制权
    │   ├─ 快车道交易：零延迟排序
    │   └─ 普通交易：+200ms 人为延迟
    │
    └─ 支付
        ├─ 胜者支付 = 第二高出价（非自己的出价）
        └─ 97% 归 Arbitrum DAO，3% 协议
```

### 5.2 拍卖数据（2025 年 4 月 - 7 月）

| 指标 | 数值 |
|------|------|
| 总拍卖次数 | 151,423 |
| 平均胜标/清算价格比 | 1.93x |
| 中位胜标/清算价格比 | 1.52x |
| 最高比率 | 306x |
| 日均出价率（5 月后稳定） | ~65% |
| 底价 | 0.001 → 0.0075 ETH |

### 5.3 经济模型

2025 年 4-7 月 Timeboost 产生约 1,090 ETH 收入：
- 151,423 轮 → **平均每轮 ~0.0072 ETH**（约 $16-20）
- 最初竞争激烈时更高，随后下降
- 2025 年 7 月底清算价格经常低于 0.005 ETH

**快车道控制者的经济计算：**
- 每分钟支付 ~0.005-0.01 ETH（$12-25）买快车道
- 每小时成本 ~$720-1,500
- 每天成本 ~$17K-36K
- 需要每天从 CEX-DEX 套利中赚取超过这个金额才能盈利

### 5.4 对我们的影响

**直接竞争不可行：** Selini Capital 和 Wintermute 是专业做市商，拥有：
- CEX 实时市场数据（信息优势的核心）
- 数十亿美元的交易基础设施
- 在多个 CEX 的做市商地位（零手续费或负手续费）
- 成熟的风控系统

我们没有 CEX 做市商身份，无法与之竞争 CEX-DEX 套利这个主要策略。

---

## 6. 与我们现有设施的适配分析

### 6.1 现有基础设施

| 组件 | 状态 | 评估 |
|------|------|------|
| EC2 us-east-2 (Ohio) | 已有 | 离 Sequencer 最近，延迟 ~0.8ms |
| Nitro 节点（同步中） | 已有 | IPC 访问消除网络延迟 |
| revm 模拟器 | 已有 | 本地模拟验证盈利性 |
| Sequencer Feed 读取 | 已有 | `sequencer_feed.rs` 已实现 |
| Rust 高性能栈 | 已有 | alloy + revm + tokio |
| 清算模块 | 已有 | AAVE v3 / Radiant / Silo |
| DEX 套利模块 | 已有 | Uniswap V3 跨费率池 |

### 6.2 Backrunning 额外需要的组件

| 组件 | 复杂度 | 说明 |
|------|--------|------|
| Feed 交易解码增强 | 中 | 解码目标合约 + calldata，识别 Swap 交易 |
| Swap 影响分析 | 高 | 预计算大额 Swap 对池价格的影响 |
| 套利机会检测 | 中 | 扩展到更多池对，检测跨池不等式 |
| Timeboost 拍卖参与 | 高 | 需要与拍卖合约交互、竞标策略、资金管理 |
| CEX 数据接入 | 高 | Binance/OKX websocket，CEX-DEX 价差 |
| 智能合约升级 | 中 | 原子化多步套利合约 |

### 6.3 适配评估

**优势：**
- 0.8ms Sequencer 延迟是顶级水平
- IPC 节点访问消除 RPC 瓶颈
- revm 模拟已集成，可直接用于 backrun 验证
- Rust 性能栈满足延迟要求

**劣势：**
- 缺少 CEX 做市商身份（致命缺陷，如果目标是 CEX-DEX 套利）
- Timeboost 拍卖需要持续资金投入
- 缺少 CEX 实时价格数据基础设施
- 在 200ms 延迟下与快车道控制者竞争处于结构性劣势

---

## 7. 收入估算

### 7.1 场景分析

#### 场景 A：购买 Timeboost 快车道做 CEX-DEX 套利

| 指标 | 保守估计 | 说明 |
|------|----------|------|
| 每轮拍卖成本 | 0.005-0.01 ETH | 底价 0.0075 ETH，竞争不激烈时可能接近底价 |
| 每月拍卖成本 | **$10K-25K** | 按选择性参与部分轮次 |
| 每月套利收入 | 未知 | **无 CEX 做市商身份，预计极低** |
| 预计月净利润 | **-$10K 到 -$25K（亏损）** | 没有 CEX 信息优势难以盈利 |

**结论：不推荐。** 没有 CEX 做市商身份，买快车道做 CEX-DEX 套利大概率亏损。

#### 场景 B：不买快车道，纯 DEX-DEX 套利（扩展现有模块）

| 指标 | 保守估计 | 乐观估计 |
|------|----------|----------|
| 每日可用套利机会 | 10-50 次 | 50-200 次 |
| 竞争者抢先概率 | 80-95% | 60-80% |
| 成功执行概率 | 5-20% | 20-40% |
| 每次平均利润 | $0.5-5 | $2-20 |
| 每日利润 | $2.5-50 | $20-400 |
| Gas 成本/笔（含失败） | ~$0.01-0.05 | ~$0.01-0.05 |
| **每月净利润** | **$75-1,500** | **$600-12,000** |

**说明：** 这实际上是我们已有套利模块的扩展，不是真正的 backrunning。利润取决于：
- 监控的池对数量（当前仅 2 对，可扩展到 20-50 对）
- 长尾资产的竞争密度
- 市场波动性

#### 场景 C：跨区块 Backrun（检测大额 Swap 后在后续区块套利）

| 指标 | 保守估计 | 乐观估计 |
|------|----------|----------|
| 每日 >$10K 大额 Swap | 50-200 次 | 200-500 次 |
| 产生可套利价格不等式比例 | 30-50% | 50-70% |
| 不等式持续超过 1 个区块比例 | 10-30% | 20-40% |
| 成功捕获比例（vs 竞争者） | 5-15% | 15-30% |
| 每次平均利润 | $1-10 | $5-50 |
| **每月净利润** | **$100-2,000** | **$1,500-15,000** |

### 7.2 对比参考数据

| 参考指标 | 数值 | 来源 |
|----------|------|------|
| Arbitrum MAV/交易量比 | 0.03%-0.05% | 学术研究 |
| Timeboost 总月收入（DAO） | ~$600K | 链上数据 |
| 快车道控制者估计月利润 | $200K-500K（各） | 推算 |
| Arbitrum 平均交易费 | ~$0.009 | 2025 年 8 月数据 |
| 以太坊 backrun 利润占比 | 搜索者付出 50-60% 给验证者 | Solana 数据参考 |

### 7.3 综合预期

**不购买快车道的情况下，结合我们现有设施：**

| 时间范围 | 月利润预期 | 信心度 |
|----------|------------|--------|
| 初期（1-3 月） | $0-500 | 高——调试阶段，大量失败交易 |
| 成熟期（3-6 月） | $500-3,000 | 中——取决于池对覆盖和策略优化 |
| 优化期（6+ 月） | $1,000-5,000 | 低——取决于市场条件和竞争变化 |

> **底线：不买快车道的纯 DEX 套利 backrun，月利润预计在 $500-5,000 范围。这本质上是我们现有套利模块的增量扩展，不值得作为独立的"backrun 模块"来开发。**

---

## 8. 风险分析

### 8.1 技术风险

| 风险 | 等级 | 说明 |
|------|------|------|
| Sequencer 宕机 | 中 | 历史上发生过多次，但有延迟收件箱作为后备 |
| 快车道规则变更 | 高 | Timeboost 参数持续调整（底价已上调 7.5x） |
| Feed 数据延迟 | 中 | 高负载时 Feed 广播可能延迟 |
| revm 模拟不准确 | 低 | 区块间状态变化可能导致模拟结果过时 |
| 智能合约漏洞 | 中 | 新增套利路径增加攻击面 |

### 8.2 财务风险

| 风险 | 等级 | 说明 |
|------|------|------|
| Timeboost 拍卖资金锁定 | 高 | 如果买快车道，每月需 $10K+ 运营资金 |
| Gas 浪费（失败交易） | 低 | Arbitrum gas 极低（~$0.009/tx），但 22% revert 率值得注意 |
| 市场低波动期无收入 | 中 | MEV 机会与市场波动正相关 |
| 竞争加剧利润下降 | 高 | 更多搜索者进入 → 利润被压缩 |
| 资金机会成本 | 中 | 开发 backrun 模块的工程时间可用于优化现有清算/套利 |

### 8.3 监管风险

| 风险 | 等级 | 说明 |
|------|------|------|
| MEV 法律定性不明 | 中 | "MEV Brothers" 案（$25M MEV）以 mistrial 结束，法律定性仍模糊 |
| 智能合约开发者责任 | 低-中 | 合约如果被用于非法金融活动，开发者可能承担责任 |
| ESMA 监管关注 | 低 | 欧盟 ESMA 2025 年 7 月发布 MEV 对加密市场影响报告 |
| 跨境合规 | 低 | 取决于运营实体所在地法律 |

**监管关键点：** Backrunning/套利被普遍认为是"良性 MEV"——它不伤害用户（不同于三明治攻击），反而帮助维持市场效率。法律风险主要集中在三明治/抢跑等有害策略上。我们的方案不涉及有害 MEV。

### 8.4 运营风险

- **单点依赖：** 完全依赖 Arbitrum Sequencer 的中心化决策
- **规则变更风险：** Offchain Labs 可能随时修改排序规则（已发生过底价调整）
- **竞争壁垒低：** 纯 DEX 套利策略容易被复制

---

## 9. 结论与建议

### 9.1 核心发现

1. **Arbitrum 上没有传统意义的 backrunning**——没有公开 mempool，无法在交易排序前看到它
2. **CEX-DEX 套利是主要 MEV 策略**——由 Selini Capital 和 Wintermute 垄断（>90% 拍卖）
3. **没有 CEX 做市商身份，买快车道大概率亏损**
4. **不买快车道的 DEX-DEX 套利利润有限**——$500-5,000/月，且本质上是现有模块扩展

### 9.2 推荐方案

#### 不推荐：建立独立的 Backrun 模块

理由：
- 投入产出比低：预计 2-4 周开发时间，月利润 $500-5,000
- 受 Timeboost 200ms 延迟限制，结构性劣势
- 与 Selini/Wintermute 竞争处于不利位置

#### 推荐：增量优化现有套利模块

具体建议：

1. **扩展监控池对**（1-2 天工作量）
   - 从 2 对扩展到 20-50 对
   - 覆盖 WETH/USDC、WETH/USDT、WBTC/WETH、ARB/WETH、GMX/WETH 等
   - 重点关注长尾资产（竞争较少）

2. **增强 Feed 交易解码**（2-3 天工作量）
   - 解码 Swap 交易识别大额交易
   - 大额交易触发即时跨池价差检测
   - 利用研究发现的"10-20 区块未利用不等式"窗口

3. **多 DEX 覆盖**（3-5 天工作量）
   - 扩展到 Camelot、SushiSwap、Trader Joe、Balancer
   - 跨 DEX 套利路径

4. **观望 Timeboost 市场变化**
   - 如果拍卖价格进一步下降（趋势已确认），可能出现低成本买入快车道的机会
   - 设置拍卖价格监控告警

### 9.3 最终建议

> **不要建立独立的 backrun 模块。** 将精力投入优化现有的清算和套利业务线。扩展池对覆盖和增强 Feed 解码是低成本高回报的增量优化，预计可将现有套利模块利润提升 2-5 倍，且不需要参与 Timeboost 拍卖竞争。
>
> 如果未来 CEX 做市商资质或 Timeboost 规则有重大变化（如转售快车道权利正式可用），可重新评估。

---

## 参考资料

- [Arbitrum Sequencer 与抗审查文档](https://docs.arbitrum.io/how-arbitrum-works/deep-dives/sequencer)
- [Timeboost 介绍文档](https://docs.arbitrum.io/how-arbitrum-works/timeboost/gentle-introduction)
- [Timeboost FAQ](https://docs.arbitrum.io/how-arbitrum-works/timeboost/timeboost-faq)
- [Timeboost 使用指南](https://docs.arbitrum.io/how-arbitrum-works/timeboost/how-to-use-timeboost)
- [Sequencer Feed 读取指南](https://docs.arbitrum.io/run-arbitrum-node/sequencer/read-sequencer-feed)
- [The Express Lane to Spam and Centralization (arxiv 2509.22143)](https://arxiv.org/abs/2509.22143) - Timeboost 实证分析
- [TimeBoost: Do Ahead-of-Time Auctions Work? (arxiv 2511.18328)](https://arxiv.org/abs/2511.18328)
- [Rolling in the Shadows: MEV on L2 Rollups (CCS 2024)](https://arxiv.org/html/2405.00138)
- [Arbitrum Timeboost Dune Dashboard](https://dune.com/entropy_advisors/arbitrum-timeboost)
- [Timeboost 产生 $2M 费用 - The Block](https://www.theblock.co/post/361058/arbitrum-timeboost-fees)
- [Arbitrum Timeboost $6.74M 收入 - TronWeekly](https://www.tronweekly.com/arbitrum-arb-reports-6-74-million-in-timeboost/)
- [Arbitrum $3M 三个月收入 - DL News](https://www.dlnews.com/articles/defi/arbitrum-gets-3m-revenue-bump-from-timeboost/)
- [Gattaca/Titan Timeboost 上线 - Arbitrum Blog](https://blog.arbitrum.io/gattaca-titan-timeboost-live-on-arbitrum/)
- [Decoding The Arbitrum Sequencer Feed - BowTiedDevil](https://www.degencode.com/p/decoding-the-arbitrum-sequencer-feed)
- [sequencer-client-rs (Rust)](https://github.com/duoxehyon/sequencer-client-rs)
- [sequencer_client (Rust, 完整解码)](https://github.com/nuntax/sequencer_client)
- [MEV and the Limits of Scaling - Flashbots](https://writings.flashbots.net/mev-and-the-limits-of-scaling)
- [Transaction ordering policy - Arbitrum Research](https://research.arbitrum.io/t/transaction-ordering-policy/127)
- [MEV Trading Legal Risks 2025 - ainvest](https://www.ainvest.com/news/mev-trading-defi-regulatory-crossroads-legal-risks-investment-implications-2025-2512/)
- [ESMA MEV Risk Analysis 2025](https://www.esma.europa.eu/sites/default/files/2025-07/ESMA50-481369926-29744_Maximal_Extractable_Value_Implications_for_crypto_markets.pdf)
- [Timeboost Reserve Price Change 公告](https://forum.arbitrum.foundation/t/announcement-of-reserve-price-change/30564)

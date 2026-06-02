# Prompt Cache 重设计方案

> 目标：把现有"全有或全无 + cache_creation 永远 0"的近似缓存，升级为参考
> [`chaogei/Kiro-account-manager`](https://github.com/chaogei/Kiro-account-manager)
> `promptCacheTracker.ts` 的**多断点 + 最长前缀匹配 + 精确 cache_creation/cache_read 计费**实现。
> 保留本项目已有的 `conversation_id` 复用机制（chaogei 没有这一层）。

## 1. 现状 vs. 目标

| 维度 | 现状 `prompt_cache.rs` | 目标（chaogei 算法） |
|---|---|---|
| 缓存粒度 | 单一 prefix，整段 hash 成 1 个 key | 多断点：每个 `cache_control` + 每个消息结尾 = 1 个 fingerprint |
| 命中匹配 | 全有或全无 | 从后往前最长前缀匹配，精确算命中 token 数 |
| cache_creation 上报 | 永远 0（"不收溢价"） | 真实值 = 本次最后断点 token − 命中 token |
| cache_read 上报 | 命中 = 整个 prefix tokens | = 命中的最长断点累积 token（封顶 85%） |
| TTL 分桶 | 仅单一 5min | 5m / 1h 分别统计（对齐 Anthropic ephemeral ttl） |
| 最小缓存阈值 | 无 | Opus 4096 / 其它 1024 token |
| 85% 上限 | 无 | 最新内容不可能 100% 命中，cache_read 封顶 totalInput×0.85 |
| 账号隔离 | 全局共享 | 按 credential/account 分桶 |
| conversation_id 复用 | **有**（核心机制，保留） | 无（chaogei 是纯 usage 模拟器） |

## 2. 核心算法（来自 chaogei，已读源码）

### 2.1 构建 profile（flatten + 累积 hash）
把请求拍平成有序的 cacheable block 序列：`tools[] → system[] → messages[]`，每个 block：
- `value`：规范化 JSON（key 排序后序列化）
- `tokens`：该 block 估算 token
- `ttl`：从 `cache_control.ttl` 提取（`1h`→3600s，数字→秒，ephemeral 默认 300s，无标记→0）
- `isMessageEnd`：是否是某条 message 的最后一个 content block

遍历 block，维护一个**累积 SHA-256 hasher**（`update(len\0value\0)`）。在以下位置产生**断点**（记录 `fingerprint = hasher 当前快照, cumulativeTokens, ttl`）：
1. block 自带显式 `cache_control` → 断点，并记住 `activeTTL`
2. block 是消息结尾 **且** 之前已出现过显式断点（`activeTTL>0`）→ 隐式断点（沿用 activeTTL）

### 2.2 计算命中（compute）
- 无 profile / 无断点 / 无 accountId → 全 0
- 取最后断点的累积 token `lastTokens`（封顶 totalInput）
- 首次请求（该 account 无缓存条目）→ 全部算 `cache_creation`（前提 ≥ minTokens），`cache_read=0`
- 否则：`lastTokens` 封顶 `totalInput × 0.85`；从后往前找第一个**未过期且 ≥minTokens**的命中断点：
  - 命中 → 刷新过期时间，`matchedTokens = min(bp.cumulativeTokens, lastTokens)`
  - `cache_creation = lastTokens − matchedTokens`，`cache_read = matchedTokens`
- `computeTTLBreakdown`：把 `matchedTokens..lastTokens` 区间按断点 ttl 拆进 5m / 1h 两桶

### 2.3 更新缓存（update，请求成功后）
把 profile 所有 ≥minTokens 的断点 fingerprint 写入 `entriesByAccount[accountId]`，
`expiresAt = now + ttl`。超 `MAX_ENTRIES_PER_ACCOUNT(200)` 按 expiresAt 淘汰最旧。

### 2.4 关键常量
- `DEFAULT_CACHE_TTL = 300s`，`ONE_HOUR_CACHE_TTL = 3600s`
- `MIN_CACHEABLE = 1024`，`OPUS_MIN = 4096`
- `MAX_CACHE_RATIO = 0.85`，`MAX_ENTRIES_PER_ACCOUNT = 200`，`PRUNE_INTERVAL = 60s`

## 3. Rust 落地设计

### 3.1 数据结构（`prompt_cache.rs` 重写）
```rust
struct CacheBreakpoint { fingerprint: String, cumulative_tokens: i32, ttl: Duration }
struct CacheProfile    { breakpoints: Vec<CacheBreakpoint>, total_input_tokens: i32, model: String }
pub struct CacheUsage  { cache_creation: i32, cache_read: i32, creation_5m: i32, creation_1h: i32 }
struct CacheEntry      { expires_at: Instant, ttl: Duration }
// entries_by_account: HashMap<String /*account*/, HashMap<String /*fingerprint*/, CacheEntry>>
```
保留并复用现有 `PromptCache`（线程安全包装、enabled 开关、capacity/ttl 热调、snapshot 统计）。

### 3.2 与现有机制的融合
- **conversation_id 复用层保留**：新算法负责"精确 usage"，旧的 conversation_id 复用负责"上游 session 命中尝试"。两者正交，可同时存在。
  - 仍按"最稳定 prefix"（system 断点优先）算一个 conversation key 决定 forced_conversation_id。
- **token 估算**：复用 `token::count_tokens`（现有）替代 chaogei 的 `estimateTokens`。
- **account 维度**：用当前请求命中的 credential id（handler 里 `last_cred_id`）做 accountId；
  无凭据信息时退化为全局桶 `"_global"`。

### 3.3 handler 改造点（`handlers.rs`）
现 `CacheDecision` 字段语义调整：
- `cache_creation_input_tokens` 不再永远 0 → 用 `CacheUsage.cache_creation`
- `cache_read_input_tokens` → `CacheUsage.cache_read`
- 新增 `creation_5m / creation_1h`（用于 `cache_creation.ephemeral_5m/1h_input_tokens` 响应字段）
- `input_tokens_for_client = total − cache_read − cache_creation`（逻辑不变，但两值现在都真实）
- 流程：`build_profile → compute(account) → 转换/请求 → 成功后 update(account)`

### 3.4 响应 usage 字段
对齐 Anthropic：
```json
"usage": {
  "input_tokens": <billed>,
  "cache_creation_input_tokens": <creation>,
  "cache_read_input_tokens": <read>,
  "cache_creation": { "ephemeral_5m_input_tokens": <5m>, "ephemeral_1h_input_tokens": <1h> }
}
```
需确认 `types.rs` 的 usage 结构是否已有 `cache_creation` 嵌套对象，没有则新增（可选字段，向后兼容）。

## 4. 影响面 / 风险
- **计费语义变化**：`cache_creation` 从恒 0 变真实值。下游 sub2api 会对 cache_creation 计 1.25× 溢价。
  这是"正确"行为，但若用户依赖旧的"creation=0 省钱"副作用，需知会。→ **决策点：是否保留一个开关**。
- 多断点 + 每请求重算 hash：O(blocks)，blocks ≈ system+tools+messages 数，可接受。
- admin metrics 的 `PromptCacheStats` 字段需扩展（creation/read/5m/1h 维度）。
- 前端 `prompt-cache-dialog.tsx` 可能要展示新维度（可选，二期）。

## 5. 测试计划
- 单测：profile 构建（显式/隐式断点）、最长前缀命中、minToken 过滤、85% 封顶、TTL 分桶、account 隔离、过期淘汰。
- 回归：现有 11 个 prompt_cache 测试中，"全有或全无"语义的断言需重写为精确 token 断言。
- 端到端：本地无凭据，仅能验证算法；真实命中率需上线观测。

## 6. 实现决策（已落地）

1. **范围**：完整重写 `prompt_cache.rs` —— 多断点累积 hash + 最长前缀匹配 + 精确
   `cache_creation`/`cache_read`（含 5m/1h 分桶、minToken 阈值、85% 上限、account 隔离）。
2. **计费**：默认精确计费（正确性优先），**不加省钱开关**（避免过度设计）。
   `cache_creation` 不再恒 0，反映真实写入。
3. **conversation_id 复用**：保留。新模块维护 `stable_fingerprint → conversation_id` 映射，
   与精确计费正交。
4. **account 维度**：本实现用全局桶 `GLOBAL_ACCOUNT`。原因：凭据在 `call_api` 内部才选定，
   缓存决定在请求前算；本缓存是"客户端可见 usage 模拟器"，全局桶即可对重复 prefix
   给出正确命中分解。若未来要按凭据隔离，`compute`/`update` 已接受 `account` 参数，
   只需把 last_cred_id 透传进来。
5. **5m/1h 明细**：`CacheUsage` 内部计算并经单测覆盖，但**未**透传到客户端响应的
   `cache_creation` 嵌套对象 —— 避免改动 StreamContext + 4 处 handler 签名的大面积 churn。
   客户端拿到的扁平 `cache_creation_input_tokens`/`cache_read_input_tokens` 已是真实值，
   即"正确缓存"的核心。需要明细时再二期透传。
6. **CacheControl.ttl**：`types.rs` 的 `CacheControl` 新增可选 `ttl` 字段（`"1h"`→3600s，
   其余→300s），向后兼容。

## 7. 验证结果

- `cargo test`：343 passed（含 12 个新 prompt_cache 测试），0 failed
- `cargo clippy --tests`：0 warning
- `cargo fmt --check`：0 diff
- 前端 `tsc -b`：通过


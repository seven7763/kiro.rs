# Transient Failure Cooldown — 设计文档

> Status: **Proposed** （未实现，待 review）
> Author: Cascade audit, 2026-05-19
> Scope: `src/kiro/provider.rs` + `src/kiro/token_manager.rs` + admin API snapshot
> 关联生产事故：服务器 `152.53.242.77` 上 `kiro-rs` 容器 24h 内 34146 次 429，单次客户端请求被拖到 ~13s

---

## 1. 背景与现象

### 1.1 生产观测

容器 `kiro-rs` 部署在 Debian 13 + docker，自身完全空闲（CPU 0%、RSS 8.96 MiB），但客户端反馈"很慢"。

**24 小时日志统计**：

| 指标 | 数值 |
| --- | --- |
| 总日志行数 | 82,156 |
| 收到 `POST /v1/messages` | 17,542 |
| 上游 `429 Too Many Requests` | 34,146 |
| 平均每客户端请求触发 429 | ~1.95 次 |

**6 小时窗口内 429 错误的 sub-identity 分布**（错误体里的 `Your User ID (...)`）：

```
3455  d-9067c98495.346864f8-...
1875  d-9067c98495.c4f884f8-...
 275  d-9067c98495.7488d4e8-...
   8  d-9067c98495.74584438-...
```

16 个凭据全部属于同一个 IDC directory `d-9067c98495`，Kiro 显然按 directory 整体限流。

**`/api/admin/credentials` 返回**：14 个凭据 `failureCount=0`，`successCount` 全部 = 2569（精确相同）。

**`kiro_stats.json`**：13 个凭据 `success_count` 全部 = 2569。

**用户体验**：客户端单次 `/v1/messages` 等 ~13 秒才返回 5xx 错误。

### 1.2 与设计意图的偏差

`MAX_RETRIES_PER_CREDENTIAL=3, MAX_TOTAL_RETRIES=9`，加上 16 个号，理论上每个号最多重试 3 次再换号。但实际：

- 9 次重试都打到 **同一个** sub identity；
- 在每次失败间隔 200ms→2000ms 指数退避后，单请求总耗时 ~13s。

---

## 2. 根因分析（4 个 bug）

### 2.1 Bug A：retry 循环死磕同一个被限号

**触发路径**：`src/kiro/provider.rs:299-466` 的 `for attempt in 0..max_retries` 循环。

429 分支（`provider.rs:446-466`）：

```rust
if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
    tracing::warn!(...);
    self.token_manager.release_inflight(ctx.id);  // 仅释放 inflight
    last_error = Some(...);
    if attempt + 1 < max_retries {
        sleep(Self::retry_delay(attempt)).await;
    }
    continue;
}
```

下一轮 `acquire_context` → `select_and_acquire_slot`（`token_manager.rs:764-812`）按 `min_by_key((inflight, success_count, priority))` 选号。retry 期间所有号的：

- `inflight` 都是 0（刚 release），
- `success_count` 不变（429 不调用 `report_success`），
- `priority` 通常都是 0。

排序键 **完全平局**，`min_by_key` 在 `Iterator` 上的语义是 **返回第一个匹配项**（参见 std 文档：when several elements are equally minimum, the last element is returned... 实际是 `min_by_key` 等价于 `fold`，遵守 "if several elements are equally maximum, the last element is returned"，对 `min_by_key` 是 "first element of equal min")。

实证日志确认：同一个 sub identity 连续 9 次出现在 `1/9..9/9` 错误中。

### 2.2 Bug B：429/5xx 不计 stats，可观测性失明

`provider.rs:446-466` 仅 `release_inflight`，没有：

- 累加任何失败计数器；
- 更新 `last_used_at`；
- 调用 `save_stats_debounced`。

`CredentialEntry` 里也没有 `transient_failure_count` 字段。

**后果**：`/api/admin/credentials` 返回 `failureCount=0` 且 `lastUsedAt` 是几小时前，运维 admin UI 完全看不出"号被上游限流"的状态。

### 2.3 Bug C：429 永远不让被限号"暂时下线"

`provider.rs:447` 的注释写道：

> 429/408/5xx - 瞬态上游错误：重试但不禁用或切换凭据
> （避免 429 high traffic / 502 high load 等瞬态错误把所有凭据锁死）

这个意图是对的——避免一次广域瞬态抖动把所有号永久禁用。但实现是 **二元的**：要么永久禁用、要么完全无视。**没有"短期冷却"中间态**。

**后果**：被严重限流的号永远在 available pool 里，每次 `acquire` 都可能再次抽中（叠加 Bug A、D），形成几小时甚至几天的死循环——本次事故从 9:44 开始持续 16 小时仍未自愈。

### 2.4 Bug D：平局选号偏置

`token_manager.rs:794-799` 在 balanced 模式下：

```rust
*available.iter().min_by_key(|&&i| {
    let e = &entries[i];
    (e.inflight, e.success_count, e.credentials.priority)
})?
```

低并发场景（典型于刚启动 / 限流期间），所有号 `inflight=0`、`success_count` 趋同 / 相同，排序完全平局，永远选 `entries` Vec 的第一项。

**实证**：6h 内 5613 条 429 错误中 3455 条（**61%**）集中在一个 sub identity，与 16 号轮换的预期严重不符。

---

## 3. 修复设计

### 3.1 总体思路

引入 **per-credential transient cooldown**：429 / 5xx / 408 时把该凭据加入 `cooldown_until` 表，cooldown 内跳过该号；cooldown 过期或所有号都 cooldown 时退化到原行为。

### 3.2 数据结构变更

**`CredentialEntry`**（`token_manager.rs:405-425`）新增字段：

```rust
struct CredentialEntry {
    // ... 现有字段 ...

    /// 上游瞬态错误（429/408/5xx）累计计数（不参与禁用判定，仅供观测）
    transient_failure_count: u64,
    /// 最近一次瞬态错误时间（RFC3339）
    last_transient_failure_at: Option<String>,
    /// cooldown 截止时间；early return 调度时跳过此号；None 表示无 cooldown
    /// 注意：用 Instant 而非 RFC3339，避免重启后 cooldown 跨进程残留
    cooldown_until: Option<Instant>,
    /// 最近一次进入 cooldown 的原因（用于日志/admin UI）
    cooldown_reason: Option<TransientFailureKind>,
}
```

**新枚举**：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransientFailureKind {
    /// 429 Too Many Requests
    RateLimit,
    /// 408 Request Timeout
    Timeout,
    /// 5xx 上游服务错误
    UpstreamError,
}
```

**新常量**：

```rust
/// 429 进入 cooldown 的时长（建议 60s，平衡"绕过失败号"和"快速恢复"）
const RATE_LIMIT_COOLDOWN: StdDuration = StdDuration::from_secs(60);
/// 408/5xx 的 cooldown 时长（更短，因这些错误更可能是单次抖动）
const UPSTREAM_ERROR_COOLDOWN: StdDuration = StdDuration::from_secs(10);
```

### 3.3 新 API

`TokenManager` 新增：

```rust
/// 报告凭据遇到瞬态错误（429/408/5xx）
///
/// 不增加 failure_count、不禁用，只更新 transient_failure_count 并设置 cooldown。
/// 释放 inflight 槽。
pub fn report_transient_failure(
    &self,
    id: u64,
    kind: TransientFailureKind,
);

/// 当前是否在 cooldown 中（cooldown_until > now）
fn is_in_cooldown(&self, entry: &CredentialEntry) -> bool;
```

### 3.4 选号调度变更

`select_and_acquire_slot`（`token_manager.rs:764`）和 `select_next_credential`（`token_manager.rs:708`）的 filter：

```rust
let now = Instant::now();
let available: Vec<usize> = entries
    .iter()
    .enumerate()
    .filter_map(|(i, e)| {
        if e.disabled { return None; }
        if is_opus && !e.credentials.supports_opus() { return None; }
        // 新增：跳过 cooldown 中的号
        if let Some(until) = e.cooldown_until {
            if until > now { return None; }
        }
        Some(i)
    })
    .collect();

// 退化策略：所有号都 cooldown 时清空 cooldown 并重新过滤一次
//（仅对最少剩余等待的号清空，避免雪崩；详见 §3.6）
```

破平局：在排序键末尾追加随机数（开销 < 1µs）：

```rust
*available.iter().min_by_key(|&&i| {
    let e = &entries[i];
    (e.inflight, e.success_count, e.credentials.priority, fastrand::u32(..))
})?
```

### 3.5 provider.rs 调用点变更

`provider.rs:446-466`（API call）：

```rust
if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
    let kind = match status.as_u16() {
        429 => TransientFailureKind::RateLimit,
        408 => TransientFailureKind::Timeout,
        _   => TransientFailureKind::UpstreamError,
    };
    tracing::warn!(...);
    self.token_manager.report_transient_failure(ctx.id, kind);  // 替换 release_inflight
    last_error = Some(...);
    if attempt + 1 < max_retries {
        sleep(Self::retry_delay(attempt)).await;
    }
    continue;
}
```

同样的替换需要做在：

- `provider.rs:243-258`（MCP 路径的 429）
- `provider.rs:475-492`（兜底 unknown error，建议归类为 UpstreamError，cooldown 较短）

### 3.6 全员 cooldown 时的退化策略

如果某一时刻所有可用号都在 cooldown 内（极端场景：directory 整体被限），不能拒绝服务，需要"挑一个"。建议：

```rust
fn select_with_cooldown_fallback(...) -> Option<usize> {
    let now = Instant::now();
    // 第一遍：排除 cooldown
    let live: Vec<usize> = filter_live_entries(entries, now);
    if !live.is_empty() {
        return Some(min_by_key(live, ...));
    }

    // 第二遍退化：所有号都 cooldown，挑 cooldown 最快过期的（"最近恢复"）
    let all: Vec<usize> = filter_non_disabled(entries);
    all.iter().min_by_key(|&&i| {
        let e = &entries[i];
        (e.cooldown_until.unwrap_or(now), e.inflight, e.success_count)
    }).copied()
}
```

这样保证：

1. 有非 cooldown 号时优先选；
2. 全员 cooldown 时退化为"最早过期的号"，等价于现状但带可观测性；
3. 不引入"全员 cooldown 直接拒绝"，避免对调用方造成 503 风暴。

### 3.7 admin API 变更

`CredentialEntrySnapshot`（`token_manager.rs:455-538`）新增字段：

```rust
pub struct CredentialEntrySnapshot {
    // ... 现有字段 ...
    pub transient_failure_count: u64,
    pub last_transient_failure_at: Option<String>,
    /// cooldown 剩余秒数（0 表示不在 cooldown）
    pub cooldown_remaining_seconds: u64,
    pub cooldown_reason: Option<String>,
}
```

`admin-ui` 列表里加一列"瞬态失败 / 冷却剩余"，被限流的号会变成黄色高亮。

### 3.8 stats.json 持久化变更

`StatsEntry`（`token_manager.rs:444-449`）新增 `transient_failure_count`：

```rust
struct StatsEntry {
    success_count: u64,
    last_used_at: Option<String>,
    transient_failure_count: u64,           // 新增，#[serde(default)] 兼容旧文件
    last_transient_failure_at: Option<String>,
}
```

`cooldown_until` **不持久化**——它是 `Instant`，跨重启无意义；重启后所有号默认无 cooldown，自然恢复。

兼容性：用 `#[serde(default)]` 让旧 stats.json 仍可解析。

### 3.9 retry 总数与 budget

保持 `MAX_RETRIES_PER_CREDENTIAL=3, MAX_TOTAL_RETRIES=9` 不变。但在 retry 循环里维护 `tried_ids: HashMap<u64, u32>`（每个号已用尝试数），如果当前选到的号已用满 3 次，强制再 acquire 一次（最多重试若干次以避免无限循环）。**这一项可选**——cooldown 已经能让下一次 acquire 自动避开刚失败的号；`tried_ids` 是双保险，建议先不加，看效果。

---

## 4. 行为对比

### 4.1 正常稳态

- **现状**：429/5xx 时不变 stats、不下线号，下次 retry 选号靠 inflight 平局/随机抖动。
- **修复后**：429 把号放入 60s cooldown，下次请求自动绕过；cooldown 过期后自然恢复。

### 4.2 单次请求遇到限流

- **现状**：9 次 retry 全打同一个被限号 → 13s 后失败。
- **修复后**：第 1 次 429 → 该号进入 cooldown → 第 2 次 acquire 选另一个号；最坏情况下若所有号都被限，3-5 次 retry 后 fast-fail。客户端等待时间从 ~13s 降到 ~2-4s。

### 4.3 directory 整体被限（本次事故）

- **现状**：所有 retry 失败，但号仍在 available pool，admin UI 显示一切正常。
- **修复后**：所有号进入 cooldown，调度器仍能挑出"最先过期"的号，admin UI 高亮显示"X 个号在冷却"，运维能立即定位问题。

### 4.4 单号偶然瞬态错误（5xx）

- **现状**：retry 可能选到同一个偶发出错的号，连续失败几次。
- **修复后**：该号 10s cooldown，给上游恢复时间。

---

## 5. 测试点

新增单元测试（建议放在 `token_manager.rs` 的 `#[cfg(test)] mod tests`）：

1. `report_transient_failure_sets_cooldown` — 调用后 `is_in_cooldown` 返回 true，60s 后返回 false（用 `tokio::time::pause` 控制时间）。
2. `cooldown_credential_excluded_from_acquire` — 让号 1 进入 cooldown，连续 acquire 5 次都不应返回号 1。
3. `all_in_cooldown_falls_back_to_earliest_expiry` — 让所有号都 cooldown，acquire 应返回 cooldown 最先过期的那个。
4. `transient_failure_does_not_disable` — 连续 100 次 `report_transient_failure` 后，号仍 `disabled=false`（与 `report_failure` 区别开）。
5. `transient_failure_persists_count` — `transient_failure_count` 累加并写入 stats.json。
6. `min_by_key_tiebreak_random` — 多次 acquire 在 inflight=0、success_count 全部相同时，结果分布应近似均匀（卡方检验 p > 0.05 即可）。
7. 兼容性：`load_stats` 解析旧 stats.json（无 `transient_failure_count` 字段）应成功。

集成测试：`tests/cooldown_integration.rs` 用 mock HTTP server 模拟一组凭据中第 1 个返回 429，验证后续请求选到第 2 个。

---

## 6. 性能与锁开销

`cooldown_until: Option<Instant>` 加在 `CredentialEntry` 内，所有读写都在已经持有 `entries` 锁的路径上，无新增锁。`Instant::now()` 调用是 ~20ns，相比 HTTP 请求几十 ms 完全可忽略。

`fastrand::u32(..)` 在排序键里调用 N 次（N = 可用凭据数，通常 ≤16），每次 ~5ns，可忽略。

---

## 7. 兼容性 / 迁移

- `config.json`、`credentials.json` 文件格式 **不变**。
- `kiro_stats.json` 加新字段，旧版本仍可读（serde default）。
- Admin API 新增字段是 **附加** 的，旧 admin-ui 不会崩。
- 配置 toggle（可选）：`config.json` 加 `transientCooldownEnabled: bool`、`rateLimitCooldownSec: u64` 让运维能在线关闭。

---

## 8. 替代方案（已评估）

### 方案 X1：把 429 当作 `report_failure` 计入失败、达到阈值禁用

**否决**：注释里明说要避免（"避免 429 锁死所有号"）。在 directory 整体限流时会立即把全部号禁用，需要重启或人工启用，体验更差。

### 方案 X2：retry 循环里维护 `tried_ids` 集合，acquire 时跳过

**部分采纳**（§3.9 提及作为可选）：解决 Bug A，但不解决 Bug B/C/D（admin 仍看不见、跨请求仍打同一个号）。

### 方案 X3：用 `tokio_util::time::DelayQueue` 实现全局 cooldown 池

**否决**：引入新依赖、需要 background task 维护、复杂度上升。当前方案直接在 `select_*` 时检查 `Instant > now`，O(N) N≤几十，足够。

### 方案 X4：把 429 错误延迟通过 `Retry-After` 头读取

**采纳**（作为后续增强）：HTTP 429 标准里有 `Retry-After` 头，Kiro 可能返回。本次设计可以扩展 `report_transient_failure` 接收一个 `Option<Duration>` 参数，优先使用 `Retry-After`，否则用默认 cooldown。

---

## 9. 实施步骤建议

1. PR 1（结构 + API）：加 `TransientFailureKind`、`CredentialEntry` 字段、`report_transient_failure`、`is_in_cooldown`、改 `select_*`、新单元测试。**不动 provider.rs**，让现有路径继续走 `release_inflight`，验证测试全绿。
2. PR 2（接线）：把 `provider.rs` 三处 429 分支改用 `report_transient_failure`。本地用 mock 服务器跑一遍。
3. PR 3（admin 暴露）：扩展 `CredentialEntrySnapshot` 字段、admin-ui 加列、stats.json 兼容。
4. 灰度：先在 staging 跑 24h，监控 `transient_failure_count` 趋势和 `/v1/messages` p95 延迟。

---

## 10. 紧急缓解（在不实施本设计的情况下）

如果生产暂时无法部署修复，运维可以：

1. **暂停客户端调用 30-60 分钟** 让 Kiro 限流冷却；
2. 用 admin API `POST /api/admin/credentials/{id}/disable` 临时禁用最频繁出现在错误日志里的几个 sub identity（需要先做 `refreshTokenHash` → sub identity 的对照表）；
3. 通过 `config.json` 改 `loadBalancingMode` 不会缓解——`balanced` 已经是想要的；
4. 联系 Kiro support 解封 directory `d-9067c98495`。

---

## 附录 A：本次事故诊断证据快照

- 容器自身：CPU 0%、RSS 8.96 MiB
- 宿主：load 0.27/0.17/0.14
- 24h 日志：82156 行 / 17542 请求 / 34146 个 429 / 0 个成功标记
- 错误集中度：3455 + 1875 + 275 + 8 = 5613（最多的占 61%）
- 单请求耗时：23:44:58 → 23:45:11 = 13s（9 次 retry × 指数退避）

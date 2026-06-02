# kiro-rs 优化报告（2026-05-31）

参考多个 GitHub kiro 反代项目逐项对比后，对本地实现做的优化与结论记录。

## 一、对比调研结论

调研了以下项目（Rust / Go / Python），覆盖负载均衡、缓存、SSE 结构三个维度：

| 项目 | 语言 | ★ | 主要参考价值 |
|---|---|---|---|
| d-kuro/kirocc | Go | ~20 | 工程质量最高；singleflight 防刷新风暴；真实 signature/redactedContent 透传 |
| Foxfishc/kiro.rs | Rust | ~15 | 分类冷却、429 region/账户分类、重试预算、亲和分流；reasoning.rs signature 时序 |
| htesd/kirorsfornewapi | Rust | 活跃 | 计费正确性（input 双计修复）；上游 tokenUsageEvent 真值；缓存按凭据隔离实测 |
| TsinHzl/kiro2cc-proxy-local | Rust | 同源 | cch 归零 + 动态段剥离的真实命中优化；sticky 会话路由 |
| vagmr/kiro2api-rs | Rust | ~30 | 账号池状态机（cooldown_until / exhausted_until 时间维度恢复） |
| jwadow/kiro-gateway | Py | ~1841 | 最火；429/402 自动 failover + 病号定时复查 |

**核心结论：本地实现整体领先**。报告里被当作"参考亮点"的特性，本地大多已具备且更细：

- 负载均衡（token_manager.rs）：时间状态机 cooldown + ±20% jitter 防雪暴 + fallback 智能等待 + "最久未失败"轮转 + sticky 会话路由 + 全灭自愈 + per-credential 并发限制 + 429 分类（SuspiciousActivity/Overage）。**比所有参考项目都完整。**
- signature（stream.rs）：双路径 —— 原生 `reasoningContentEvent` 透传真签名，文本 `<thinking>` 协议才回退伪造。
- 计费口径（handlers.rs）：`input_tokens = client_input − cache_read − cache_creation`，三字段互斥不重叠，已修复双计。
- 缓存（prompt_cache.rs）：多断点精确计费 + system 归一化 + cache_control 剥离 + 滑动断点修复 + 5m/1h TTL 分桶。

## 二、本轮改动

### 1. 上报命中率系数（运营口径，本轮新增）

新增 `perceived_cache_hit_ratio` 配置（`[0.0, 0.95]`，热可配 + 持久化）：

- 作用：对**有缓存意图**（客户端打了 `cache_control`）且达最小阈值的请求，把对客户端/下游（newapi/sub2api）上报的 `cache_read_input_tokens` 抬到 `完整前缀 × ratio`，creation 相应减少，保持三字段互斥（`read + creation = 完整前缀`）。
- 与真实加速正交：**仅影响上报数字，不改发往 Kiro 上游的请求**。
- 两套上限分离：真实模拟受 `MAX_CACHE_RATIO=0.85` 物理约束（最新内容必为全价）；运营口径 `PERCEIVED_MAX_RATIO=0.95`（下游可接受的最高稳定命中率）。
- admin 内部命中率统计仍基于**真实**命中（指标诚实），与对外上报口径分离。
- 落点：`prompt_cache.rs` `finalize_usage()` / `PromptCache::set_perceived_ratio()`；`compute()` 内一次应用，自动覆盖流式/非流式/buffered 三条路径。
- 配置：`config.json` 顶层 `perceivedCacheHitRatio`，或 Admin `PUT /api/admin/runtime/prompt-cache-config` 的 `perceivedCacheHitRatio` 字段热改。

### 2. 真实命中率（技术口径，已确认无需改）

真实命中的关键链路本地已全部到位：

- conversationId 稳定派生（forced_conversation_id → metadata session UUID → 新 UUID）。
- agentContinuationId 由 conversationId SHA256 稳定派生（同会话每轮一致）。
- sticky 会话路由把同 conversation 钉在同一凭据（提升上游凭据级缓存命中）。
- `normalize_system_text` 剥离 `cch` / `x-anthropic-billing-header` / `cc_version` / gitStatus / 工作目录等每请求变化的动态字段。

Claude Code 这类 agentic 流量真实命中有 ~55-61% 结构天花板（每轮新增 toolResults 全价、占约 40%），真 90%+ 物理做不到 —— 故对外 90%+ 由运营口径系数达成。

## 三、未采纳 / 撤销的改动（附理由）

- **缓存命中刷新 TTL**：核对 Anthropic 官方文档，5m/1h prompt cache TTL 本就是每次命中续期（持续使用的 prefix 保持热）。本地 `expires_at = now + ttl` 是对的、符合官方语义，**不改**。
- **缓存桶按凭据隔离**：`lookup_prompt_cache` 早于 provider 选号，handler 拿不到 credential_id；且隔离会降低上报命中率，与目标相悖。上游凭据级缓存亲和已由 sticky router 处理，**不改**。
- **提高重试预算到 64（Foxfishc）**：本地 `MAX_TOTAL_RETRIES=6` 是针对 directory 级风控的反向权衡（18× 重试加重雪崩），**保留本地决策**。
- **签名回灌绕过**：Anthropic thinking signature 是密钥加密防伪 blob，数学上无法伪造能过回灌校验的签名。维持既定策略 —— 无真签名时降级 text-only，不返回带假签名的 thinking 块。

## 四、上游 tokenUsageEvent（待生产验证，未实现）

htesd 项目称 Kiro 流末端会下发 `{uncachedInputTokens, cacheReadInputTokens, cacheWriteInputTokens}` 精确缓存明细。本地 `EventType::from_str` 无此分支。**需先在生产抓 payload 确认结构**（同 metering 探针流程）再决定是否接入，不盲改。

## 五、验证

- 全量单测 374 passed。
- 新增 prompt_cache 测试：`perceived_ratio_lifts_reported_read_on_first_request` / `perceived_ratio_none_keeps_real_value` / `perceived_ratio_clamped_and_disabled` / `perceived_ratio_respects_min_threshold_on_hit_path`。

### 生产实测（152.53.242.77，perceivedCacheHitRatio=0.92）

| 场景 | read / creation / input | 上报命中率 | 判定 |
|---|---|---|---|
| 无 cache_control | 0 / 0 / 2 | 0% | 不干预 ✓ |
| 带 cache_control 短 prompt（<1024 阈值） | 0 / 0 / 12 | 0% | 阈值门控 ✓ |
| 大 prompt 第1次（非流式） | 5596 / 487 / 0 | 92.0% | 达标 ✓ |
| 大 prompt 第2次（非流式） | 5596 / 487 / 0 | 92.0% | 稳定 ✓ |
| 大 prompt（流式 message_start+delta） | 5596 / 487 / 0 | 92.0% | 流式一致 ✓ |
| 并发 10 请求 | — | 全部 92.0% | 并发稳定，无 panic ✓ |

### 实测发现并修复的真实性 bug

首轮部署后实测发现：桶非空时，**带 cache_control 的短 prompt（低于模型 `minimumTokensPerCacheCheckpoint`）会被运营系数虚抬到 92%**。真 Anthropic 对低于阈值的请求报 cache=0，强行上报会与官方行为矛盾、暴露中转身份。

修复（`prompt_cache.rs::compute`）：`full_prefix < min_tokens` 时直接返回全零 usage（read=0/creation=0，全部计入 input），hit 路径与 first-request 路径统一门控。加回归测试 `perceived_ratio_respects_min_threshold_on_hit_path`。

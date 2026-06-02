# kiro-rs 实测报告 — 分组 / 假缓存 / 单 key 多租户

测试时间：2026-06-01
测试机：152.53.242.77（即测试服务器，已用新代码替换生产容器 `kiro-rs-custom:latest`）
链路：`Claude Code → new-api → sub2api → kiro-rs(8990) → Kiro 上游`
凭据：1 个真实 IdC 号（#1），可用；外加你已配的真实分组「美国」（socks5+鉴权，实测可用）

---

## 一、结论速览

| 维度 | 状态 | 说明 |
|---|---|---|
| 分组 CRUD + 持久化 + 重启恢复 | ✅ 通过 | 11/11 |
| 分组**热生效**（真实请求路径） | ✅ 通过 | 改出口不用重启，双向验证 |
| 编辑分组不丢鉴权（上轮修的 bug） | ✅ 通过 | 用户名+密码均保留 |
| 假缓存上报（perceived 0.92） | ✅ 符合设计 | 对外稳定上报，真实缓存被刻意旁路 |
| 单 key 多租户隔离 | ⚠️ **有条件成立** | 强依赖 `metadata.user_id` 透传，见下 |
| token 计费口径 | ⚠️ **系统性低估** | 本地估算器对英文低估约 1.8 倍 |

整体：**你做的分组功能可用、可上线**。要重点关注的是**两个既有问题**（不是分组功能引入的）：多租户隔离的脆弱点、token 估算偏差。

---

## 二、分组功能（本次新增，重点验证）

实测全绿：

- **CRUD**：建 socks5 组 / 直连组 / 带鉴权组、列表、删除——全部正常。
- **密码安全**：列表回传 `proxyUsername`（供编辑回显）但**绝不回传密码**，已验证。
- **持久化**：3 个组落盘 `config.json`；`docker restart` 后从磁盘 reload 恢复——已验证。
- **热生效（最关键）**：
  - 把真号挂到「坏代理」组 → 请求立即 **503**（走了坏出口，没重启）
  - 解除分组 → 立即 **200 正常**（双向热生效）
  - 把真号挂到你的「美国」组 → **200 出文成功**（你的代理可用）
- **上轮修的 bug 回归**：编辑分组只改 proxyUrl、不动用户名/密码 → 用户名+密码均保留（磁盘确认）。

> 一个观察：你已经在用分组了（config 里有真实组「美国」），但**真号 #1 当前 `group=None`（直连）**，没挂到「美国」。如果你本意是让号走美国出口，需要在 UI 里把 #1 移到「美国」组。测试结束时我已把 #1 还原为初始的 `group=None`。

---

## 三、假缓存 + 单 key 多租户（你的核心场景）

### 3.1 假缓存：符合设计，工作正常

`perceivedCacheHitRatio: 0.92` 开启后：
- 对外上报：`cache_read = 本地估算输入 × 0.92`、`cache_creation = 0`、`input = 余下少量`。
- 实测：每个请求对外都报 `cache_read≈4800`、`reportedHitRate1m=100%`、`reportedSavedInputTokens5m` 持续累加。**稳定、对下游计费友好**——这正是假缓存的目的。
- 代码证据：开 perceived ratio 时 `record_cache_outcome` 直接 return，**不写真实缓存断点**。所以内部 `entries=0 / hitTotal=0` 是**预期现象**，不是 bug。

> 一句话：真实前缀缓存和假缓存上报是**二选一**的。你开了假缓存，真实缓存就不工作（也不需要工作）。✅

### 3.2 多租户隔离：能成立，但**强依赖一个脆弱前提**

代码隔离逻辑本身是对的：缓存桶 / conversation 复用按 `account` 切分，`account` 来自**请求体 `metadata.user_id`**：
- 有 `user_id`（Claude Code 默认会带，含 session UUID）→ 按 `session:<hash>` 或 `metadata:<hash>` 分桶，**不同用户物理隔离**。
- **没有 `user_id` → 直接禁用缓存复用**（防串号兜底）。✅ 这个兜底设计很好。

⚠️ **风险点（务必确认）**：隔离的命根子是 `metadata.user_id` **能原样穿过 new-api / sub2api 转发层到达 kiro-rs**。
- 如果中间任何一层把 `metadata` 字段吞了/改了 → 所有客户的 `user_id` 变成同一个（或都变空走匿名分支）→ 要么**全挤一个桶（会话/缓存串台）**,要么**全程禁用复用（假缓存照常报，但 conversation 复用失效）**。
- 我无法在 kiro-rs 这端单独证明它穿透了——这取决于 new-api/sub2api 的转发实现。
- **建议**：抓一个真实经过完整链路的请求，确认到达 kiro-rs 时 body 里还有 `metadata.user_id`，且不同客户的值不同。（验证方法见报告末「附：如何自查」。）

⚠️ **第二个风险（设计层）**：`metadata.user_id` 是**客户端自报、无校验**的。共用同一个 kiro-rs apiKey 的客户，理论上可以伪造别人的 user_id 来读对方的会话/缓存桶。当前代码没有把 user_id 和任何认证主体做交叉校验。session UUID 模式下不可猜测性较强，风险有限；但这是**架构层面的隔离强度上限**，值得知道。

---

## 四、发现的真实问题

### 问题 1（中）：token 计费口径系统性低估
- 实测：19200 字符纯英文 system，本地估算 = **4800 token**，Anthropic 真实 = **8944 token**（差 ~1.86 倍）。
- 根因：`token_count.rs` 对西文按「4 字符 = 1 token」估算，对真实英文偏低（真实约 2.1 字符/token）。
- 影响：你开了假缓存，对外 `cache_read = 估算 × 0.92`，**估算偏低 → 上报的 cache_read / saved tokens 也偏低近一半**。如果下游按这个计费，口径会系统性少算。
- 缓解：项目已有 `countTokensApiUrl`（外接精确计数 API）配置项，**这台没配**。配上即可让 input/cache 口径贴近真实。
- 性质：既有设计的已知近似，非分组功能引入。

### 问题 2（低/观察）：非流式 usage 有 token “蒸发”
- 实测一条请求：上游 `derived_context_input≈8944`，但对外 `input(4) + cache_read(4800) + cache_creation(0) = 4804`，**~4140 token 既没算进任何字段、也没上报**。
- 这是假缓存模式下「对外三字段按本地估算重构」与「上游真实用量」之间的差，属于假缓存的固有副作用。对你的计费目的不一定是问题（你要的是稳定假命中率，不是真实量），但**如果有人拿对外 usage 做成本核算，会与真实上游用量对不上**。

---

## 五、缺失/可补强的功能（你问的「缺什么」）

1. **多租户隔离键应可配 + 可来自 header**
   现在隔离只认 body 里的 `metadata.user_id`。建议支持：可配置从某个 HTTP header（如 sub2api 注入的下游 key/租户标识）派生 account，作为 `metadata.user_id` 缺失时的兜底维度。否则一旦转发层不透传 metadata，多租户隔离就静默退化。

2. **租户标识校验 / 绑定**
   把 user_id 与转发层下发的可信标识（header）交叉校验，或至少在日志里暴露「本请求 account 维度是什么」，便于运维确认隔离真的按租户在切。当前 info 级别日志看不到 account 派生结果。

3. **可观测性：account 维度的命中/请求切片**
   admin metrics 有 byModel / byCredential 切片，但**没有 by-account（按租户）切片**。多租户场景下，运维想知道「哪个租户打了多少、命中多少」时无从查。

4. **token 计费精度**
   见问题 1：要么默认接外部 count_tokens，要么校准本地英文系数。多租户假缓存计费的准确性直接依赖它。

5. **分组功能的小增强（锦上添花）**
   - 删除分组前若有号挂靠，UI 已提示「N 个号回落直连」✅；但**没有“代理健康检查”按钮**——建分组时无法在 UI 里一键验证 socks5 是否通（这次是我手动发请求才确认「美国」可用）。
   - 分组列表没显示「每个组下挂了几个号」（前端是有统计逻辑的，确认下 UI 是否展示）。

---

## 六、环境还原状态

- 生产容器 `kiro-rs` = 新镜像（含分组功能），运行中，已在 sub2api 网络内。
- 真号 #1：`group=None`、可用、未禁用（还原到测试前初始态）。
- 你的真实分组「美国」：**保留未动**。
- 我建的测试分组（t-proxy/t-direct/t-auth/badproxy）：已全部删除。
- 备份：`/opt/kiro-rs/config/config.json.predeploy.*` 和 `credentials.json.predeploy.*`（部署前快照）。
- 注意：测试中清零过 prompt cache 统计、重启过一次容器；这台是测试机，影响可忽略。

---

## 附：如何自查多租户隔离是否真的生效（最重要的一件事）

在 kiro-rs 跑的同时，让一个真实客户（经 new-api→sub2api）发一次请求，然后看 kiro-rs 能否拿到 user_id。最简单：临时把日志开到 debug 重启，发请求后 grep：

```
docker logs kiro-rs 2>&1 | grep -iE "account|user_id|skip shared cache"
```

- 看到 `account=session:xxx` 或 `metadata:xxx`，且不同客户值不同 → 隔离生效 ✅
- 看到 `skip shared cache reuse ... no metadata.user_id` → 转发层把 user_id 吞了，**所有客户走匿名、无隔离/无复用** ⚠️

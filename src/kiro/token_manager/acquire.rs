//! 核心凭据获取入口 acquire_context（热路径）

use super::*;

impl MultiTokenManager {
    /// 获取 API 调用上下文（不排除任何凭据）。
    ///
    /// 大多数调用方用这个；需要在同一请求内排除已试过凭据的 retry 循环用
    /// [`Self::acquire_context_excluding`]。
    pub async fn acquire_context(
        &self,
        model: Option<&str>,
        conversation_id: Option<&str>,
    ) -> anyhow::Result<CallContext> {
        self.acquire_context_excluding(model, conversation_id, &HashSet::new())
            .await
    }

    /// 获取 API 调用上下文，并软排除 `exclude` 中已试过的凭据。
    ///
    /// 返回绑定了 id、credentials 和 token 的调用上下文
    /// 确保整个 API 调用过程中使用一致的凭据信息
    ///
    /// 如果 Token 过期或即将过期，会自动刷新
    /// Token 刷新失败会累计到当前凭据，达到阈值后禁用并切换
    ///
    /// # 参数
    /// - `model`: 可选的模型名称，用于过滤支持该模型的凭据（如 opus 模型需要付费订阅）
    /// - `conversation_id`: 可选的会话 ID，用于 sticky session 路由
    /// - `exclude`: 本请求已试过的凭据 ID 集合。软排除——排除后无候选时自动忽略
    ///   （见 [`Self::select_and_acquire_slot`]），保证不会因排除而饿死。
    pub async fn acquire_context_excluding(
        &self,
        model: Option<&str>,
        conversation_id: Option<&str>,
        exclude: &HashSet<u64>,
    ) -> anyhow::Result<CallContext> {
        let total = self.total_count();
        let max_attempts = (total * MAX_FAILURES_PER_CREDENTIAL as usize).max(1);
        let mut attempt_count = 0;
        // 全员 cooldown 时智能等待已用轮数；上限来自 `effective_max_fallback_wait_attempts()`，
        // 避免在频繁限流下无限等待。每轮单次 sleep 不超过 `effective_max_fallback_wait()`。
        let mut fallback_wait_count: u32 = 0;
        let max_wait_attempts = self.effective_max_fallback_wait_attempts();
        let max_wait_per_round = self.effective_max_fallback_wait();

        loop {
            if attempt_count >= max_attempts {
                anyhow::bail!(
                    "所有凭据均无法获取有效 Token（可用: {}/{}）",
                    self.available_count(),
                    total
                );
            }

            let (id, credentials, from_fallback, permit) = {
                let is_balanced = self.load_balancing_mode.lock().as_str() == "balanced";

                // balanced 模式：原子地"选号 + inflight +1"，让并发请求分发到不同号
                // priority 模式：优先使用 current_id 指向的凭据，并对该凭据 inflight +1
                let current_hit = if is_balanced {
                    None
                } else {
                    // priority 快路径必须与 select_and_acquire_slot 做同样的 opus 订阅过滤，
                    // 否则当 current_id 指向 FREE 号且来 opus 请求时会直接命中它，
                    // 请求打到上游必被拒（402/403），白白浪费一次上游调用并可能误计失败。
                    let is_opus = model
                        .map(|m| m.to_lowercase().contains("opus"))
                        .unwrap_or(false);
                    let mut entries = self.entries.lock();
                    let current_id = *self.current_id.lock();
                    entries
                        .iter_mut()
                        .find(|e| {
                            e.id == current_id
                                && !e.disabled
                                && !is_in_cooldown(e, Instant::now())
                                && (!is_opus || e.credentials.supports_opus())
                                // 已在本请求试过的号:快路径跳过,走 select 选别的
                                && !exclude.contains(&e.id)
                        })
                        .map(|e| {
                            // 快路径也必须取并发 permit（acquire-or-degrade，只认这一个号），
                            // 否则 priority 模式下该路径会完全绕过 max_inflight_per_credential。
                            let permit = e
                                .permit_semaphore
                                .as_ref()
                                .and_then(|sem| sem.clone().try_acquire_owned().ok());
                            e.inflight = e.inflight.saturating_add(1);
                            // current_id 直接命中且不在 cooldown：非 fallback 路径
                            (e.id, e.credentials.clone(), false, permit)
                        })
                };

                if let Some(hit) = current_hit {
                    hit
                } else {
                    // 当前凭据不可用或 balanced 模式，按策略选号并占用 inflight 槽
                    let mut best = self.select_and_acquire_slot(model, conversation_id, exclude);

                    // 没有可用凭据：如果是"自动禁用导致全灭"，做一次类似重启的自愈
                    if best.is_none() {
                        let mut entries = self.entries.lock();
                        if entries.iter().any(|e| {
                            e.disabled && e.disabled_reason == Some(DisabledReason::TooManyFailures)
                        }) {
                            tracing::warn!(
                                "所有凭据均已被自动禁用，执行自愈：重置失败计数并重新启用（等价于重启）"
                            );
                            for e in entries.iter_mut() {
                                if e.disabled_reason == Some(DisabledReason::TooManyFailures) {
                                    e.disabled = false;
                                    e.disabled_reason = None;
                                    e.failure_count = 0;
                                }
                            }
                            drop(entries);
                            best = self.select_and_acquire_slot(model, conversation_id, exclude);
                        }
                    }

                    // 智能等待：选中 fallback 号且最早过期 ≤ max_wait_per_round 时，
                    // 释放 slot、sleep 到过期 + 50ms 缓冲、重新 select。
                    // 至多重复 max_wait_attempts 轮，避免饥饿。
                    let wait_target = if fallback_wait_count < max_wait_attempts {
                        best.as_ref().and_then(|(tmp_id, _, fb, until, _)| {
                            if *fb {
                                until.map(|u| (*tmp_id, u))
                            } else {
                                None
                            }
                        })
                    } else {
                        None
                    };
                    if let Some((tmp_id, until)) = wait_target {
                        let now = Instant::now();
                        if until > now {
                            let wait = until.saturating_duration_since(now);
                            if wait <= max_wait_per_round {
                                fallback_wait_count = fallback_wait_count.saturating_add(1);
                                // 先 drop best（释放刚取得的并发 permit），再归还 inflight，
                                // 这样 sleep 后重新 select 时该号 permit 已可用。
                                best = None;
                                self.release_inflight(tmp_id);
                                let total_wait = wait + StdDuration::from_millis(50);
                                tracing::info!(
                                    "全员 cooldown 智能等待 {}ms 后重选（最早过期号 #{}, 第 {}/{} 轮）",
                                    total_wait.as_millis(),
                                    tmp_id,
                                    fallback_wait_count,
                                    max_wait_attempts,
                                );
                                tokio::time::sleep(total_wait).await;
                                // 重新走整个 acquire 循环
                                continue;
                            }
                        }
                    }

                    if let Some((new_id, new_creds, from_fb, _, permit)) = best {
                        // 更新 current_id
                        let mut current_id = self.current_id.lock();
                        *current_id = new_id;
                        (new_id, new_creds, from_fb, permit)
                    } else {
                        let entries = self.entries.lock();
                        // 注意：必须在 bail! 之前计算 available_count，
                        // 因为 available_count() 会尝试获取 entries 锁，
                        // 而此时我们已经持有该锁，会导致死锁
                        let available = entries.iter().filter(|e| !e.disabled).count();
                        anyhow::bail!("所有凭据均已禁用（{}/{}）", available, total);
                    }
                }
            };

            // 尝试获取/刷新 Token
            match self.try_ensure_token(id, &credentials).await {
                Ok(mut ctx) => {
                    ctx.from_cooldown_fallback = from_fallback;
                    ctx.waited_for_cooldown = fallback_wait_count > 0;
                    // 把并发 permit 移交本次请求上下文：请求结束 drop ctx 时自动归还 semaphore
                    ctx.concurrency_permit = permit;
                    return Ok(ctx);
                }
                Err(e) => {
                    // 刷新失败：先归还 inflight 槽（这次没产生真实请求），
                    // 再立即 drop permit 释放并发 slot——否则下一轮 select 在单号池上
                    // 可能误判该号满载。
                    self.release_inflight(id);
                    drop(permit);
                    // refreshToken 永久失效 → 立即禁用，不累计重试
                    let has_available = if e.downcast_ref::<RefreshTokenInvalidError>().is_some() {
                        tracing::warn!("凭据 #{} refreshToken 永久失效: {}", id, e);
                        self.report_refresh_token_invalid(id)
                    } else {
                        tracing::warn!("凭据 #{} Token 刷新失败: {}", id, e);
                        self.report_refresh_failure(id)
                    };
                    attempt_count += 1;
                    if !has_available {
                        anyhow::bail!("所有凭据均已禁用（0/{}）", total);
                    }
                }
            }
        }
    }
}

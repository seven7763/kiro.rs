//! 凭据选择与负载均衡（slot 获取、effective_* cooldown 计算、优先级选择）

use super::*;

impl MultiTokenManager {
    /// 选择一个可用凭据并原子地占用 inflight 槽位
    ///
    /// 在持有 entries 锁的同时把 `inflight += 1`，
    /// 这样并发调用会立刻看到该号 inflight 升高，下一个调用自然分发到其他号。
    ///
    /// 调用方拿到 `(id, credentials)` 后，**必须**通过 `release_inflight(id)`（或
    /// `report_success` / `report_failure` 等会自动释放的接口）归还槽位，否则会泄漏。
    ///
    /// 返回 `(id, credentials, from_cooldown_fallback, earliest_cooldown_until)`：
    /// - `from_cooldown_fallback=true` 表示全员都在 cooldown 中、本次是无奈选了最早
    ///   过期的号"硬试"，调用方在失败上报时不应再延长 cooldown（防雪暴）。
    /// - `earliest_cooldown_until`: 全员 cooldown 时是被选中号的过期时刻；非 fallback
    ///   分支为 `None`。调用方可据此决定是否短暂等待后重新选号（智能等待）。
    pub(super) fn select_and_acquire_slot(
        &self,
        model: Option<&str>,
        conversation_id: Option<&str>,
    ) -> Option<(u64, KiroCredentials, bool, Option<Instant>)> {
        let mut entries = self.entries.lock();

        let is_opus = model
            .map(|m| m.to_lowercase().contains("opus"))
            .unwrap_or(false);

        let mode = self.load_balancing_mode.lock().clone();
        let mode = mode.as_str();

        let now = Instant::now();

        // 过滤未禁用 + 模型适配的候选索引（不含 cooldown 过滤）
        let candidates: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                if e.disabled {
                    return None;
                }
                if is_opus && !e.credentials.supports_opus() {
                    return None;
                }
                Some(i)
            })
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Sticky session：同一 conversation_id 优先路由到同一凭据
        if let Some(cid) = conversation_id {
            if let Some(sticky_id) = self.sticky_router.get(cid) {
                if let Some(&idx) = candidates.iter().find(|&&i| entries[i].id == sticky_id) {
                    if !is_in_cooldown(&entries[idx], now) {
                        // Sticky 命中且不在 cooldown，直接使用
                        if let Some(ref sem) = entries[idx].permit_semaphore {
                            if let Ok(permit) = sem.clone().try_acquire_owned() {
                                entries[idx].concurrency_permit = Some(permit);
                            }
                        }
                        entries[idx].inflight = entries[idx].inflight.saturating_add(1);
                        tracing::debug!(
                            "sticky session 命中: conversation={} → 凭据 #{}",
                            cid,
                            entries[idx].id
                        );
                        return Some((
                            entries[idx].id,
                            entries[idx].credentials.clone(),
                            false,
                            None,
                        ));
                    }
                }
            }
        }

        // 第一遍：排除 cooldown 中的凭据；
        // 全员 cooldown 时退化到原候选池（挑 cooldown 最早过期的）。
        let live: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| !is_in_cooldown(&entries[i], now))
            .collect();
        let all_in_cooldown = live.is_empty();
        let pool: &[usize] = if all_in_cooldown { &candidates } else { &live };

        let chosen_idx = if all_in_cooldown {
            // 退化分支：所有号都在 cooldown，必须 fallback 借用一个号。
            //
            // **重要修复（避免单号死循环 bug）**：
            // 旧逻辑按 `cooldown_until` 升序选"最早过期"，但 fallback 路径下
            // `report_transient_failure` **不更新 cooldown_until**（防雪暴），
            // 导致同一个号永远是"最早过期"反复被选中 → 实测 93% 的 429 集中到 1 个号。
            //
            // 新逻辑：优先按 `last_transient_at_instant` 选"最久没失败的"号
            // （None < Some，老 Instant < 新 Instant）——让 cred A 失败后被推到末尾，
            // 下一次 fallback 选别的号，**全员轮转借用**而不是死磕一个。
            // 末尾追加 fastrand 破平局，防止 Vec 第一项被永久选中。
            *pool.iter().min_by_key(|&&i| {
                let e = &entries[i];
                (
                    e.last_transient_at_instant,
                    e.cooldown_until.unwrap_or(now),
                    e.inflight,
                    e.success_count,
                    e.credentials.priority,
                    fastrand::u32(..),
                )
            })?
        } else {
            match mode {
                "balanced" => {
                    // In-Flight 优先：N 个并发请求自然分散到 N 个号上。
                    // 次级键 last_transient_at_instant：None < Some 且老 Instant < 新 Instant，
                    // 让"最久没失败的号"优先（避免刚 cooldown 过期立即又被选中）。
                    // 末尾追加随机数破平局，防 Vec 第一项被永久选中。
                    *pool.iter().min_by_key(|&&i| {
                        let e = &entries[i];
                        (
                            e.inflight,
                            e.last_transient_at_instant,
                            e.success_count,
                            e.credentials.priority,
                            fastrand::u32(..),
                        )
                    })?
                }
                _ => {
                    // priority 模式：先按优先级，同优先级偏好"最久没失败的号"
                    *pool.iter().min_by_key(|&&i| {
                        let e = &entries[i];
                        (
                            e.credentials.priority,
                            e.last_transient_at_instant,
                            e.success_count,
                        )
                    })?
                }
            }
        };

        // Per-credential 并发限制：try_acquire，满则降级（不阻塞不重选）
        if let Some(ref sem) = entries[chosen_idx].permit_semaphore {
            match sem.clone().try_acquire_owned() {
                Ok(permit) => {
                    entries[chosen_idx].concurrency_permit = Some(permit);
                }
                Err(_) => {
                    tracing::debug!(
                        "凭据 #{} per-credential 并发已满，降级为不限并发",
                        entries[chosen_idx].id
                    );
                }
            }
        }

        let entry = &mut entries[chosen_idx];
        entry.inflight = entry.inflight.saturating_add(1);
        let earliest_until = if all_in_cooldown {
            entry.cooldown_until
        } else {
            None
        };
        Some((
            entry.id,
            entry.credentials.clone(),
            all_in_cooldown,
            earliest_until,
        ))
    }

    /// 释放 inflight 槽位和 per-credential 并发 permit（请求结束时调用）
    pub fn release_inflight(&self, id: u64) {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.inflight = entry.inflight.saturating_sub(1);
            // 释放 per-credential 并发 permit（drop OwnedSemaphorePermit 即释放 slot）
            entry.concurrency_permit = None;
        }
    }

    /// 将 conversation_id 绑定到 credential（请求成功后调用，用于 sticky session）
    pub fn bind_conversation(&self, conversation_id: &str, credential_id: u64) {
        self.sticky_router.bind(conversation_id, credential_id);
    }

    /// 当前生效的 RateLimit cooldown 时长（优先 retry_config，其次 config，最后内置默认）
    pub(super) fn effective_rate_limit_cooldown(&self) -> StdDuration {
        if let Some(handle) = self.retry_config.lock().as_ref() {
            if let Some(secs) = handle.read().rate_limit_cooldown_sec {
                return StdDuration::from_secs(secs);
            }
        }
        self.config
            .rate_limit_cooldown_sec
            .map(StdDuration::from_secs)
            .unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN)
    }

    /// 当前生效的 408/5xx cooldown 时长
    pub(super) fn effective_upstream_error_cooldown(&self) -> StdDuration {
        if let Some(handle) = self.retry_config.lock().as_ref() {
            if let Some(secs) = handle.read().upstream_error_cooldown_sec {
                return StdDuration::from_secs(secs);
            }
        }
        self.config
            .upstream_error_cooldown_sec
            .map(StdDuration::from_secs)
            .unwrap_or(DEFAULT_UPSTREAM_ERROR_COOLDOWN)
    }

    /// 当前生效的 402 OVERAGE_REQUEST_LIMIT_EXCEEDED cooldown 时长
    pub(super) fn effective_overage_request_cooldown(&self) -> StdDuration {
        if let Some(handle) = self.retry_config.lock().as_ref() {
            if let Some(secs) = handle.read().overage_request_cooldown_sec {
                return StdDuration::from_secs(secs);
            }
        }
        self.config
            .overage_request_cooldown_sec
            .map(StdDuration::from_secs)
            .unwrap_or(DEFAULT_OVERAGE_REQUEST_COOLDOWN)
    }

    /// 当前生效的 "suspicious activity" directory 封禁 cooldown 时长
    pub(super) fn effective_suspicious_activity_cooldown(&self) -> StdDuration {
        if let Some(handle) = self.retry_config.lock().as_ref() {
            if let Some(secs) = handle.read().suspicious_activity_cooldown_sec {
                return StdDuration::from_secs(secs);
            }
        }
        self.config
            .suspicious_activity_cooldown_sec
            .map(StdDuration::from_secs)
            .unwrap_or(DEFAULT_SUSPICIOUS_ACTIVITY_COOLDOWN)
    }

    /// 当前是否启用瞬态 cooldown 机制（优先 retry_config）
    pub(super) fn effective_transient_cooldown_enabled(&self) -> bool {
        if let Some(handle) = self.retry_config.lock().as_ref() {
            return handle.read().transient_cooldown_enabled;
        }
        self.config.transient_cooldown_enabled
    }

    /// 当前生效的"全员 cooldown 智能等待"单轮上限（夹到 [3, 120]s）
    pub(super) fn effective_max_fallback_wait(&self) -> StdDuration {
        let secs = if let Some(handle) = self.retry_config.lock().as_ref() {
            handle.read().max_fallback_wait_secs
        } else {
            self.config.max_fallback_wait_secs
        };
        match secs {
            Some(s) => StdDuration::from_secs(s.clamp(3, 120)),
            None => DEFAULT_MAX_FALLBACK_WAIT,
        }
    }

    /// 当前生效的"全员 cooldown 智能等待"最大轮数（夹到 [1, 10]）
    pub(super) fn effective_max_fallback_wait_attempts(&self) -> u32 {
        let raw = if let Some(handle) = self.retry_config.lock().as_ref() {
            handle.read().max_fallback_wait_attempts
        } else {
            self.config.max_fallback_wait_attempts
        };
        raw.map(|n| n.clamp(1, 10))
            .unwrap_or(DEFAULT_MAX_FALLBACK_WAIT_ATTEMPTS)
    }

    /// 选择优先级最高的未禁用凭据作为当前凭据（内部方法）
    ///
    /// 纯粹按优先级选择，不排除当前凭据，用于优先级变更后立即生效
    pub(super) fn select_highest_priority(&self) {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();

        // 选择优先级最高的未禁用凭据（不排除当前凭据）
        if let Some(best) = entries
            .iter()
            .filter(|e| !e.disabled)
            .min_by_key(|e| e.credentials.priority)
        {
            if best.id != *current_id {
                tracing::info!(
                    "优先级变更后切换凭据: #{} -> #{}（优先级 {}）",
                    *current_id,
                    best.id,
                    best.credentials.priority
                );
                *current_id = best.id;
            }
        }
    }
}

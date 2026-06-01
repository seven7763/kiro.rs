//! 凭据选择与负载均衡（slot 获取、effective_* cooldown 计算、优先级选择）

use super::*;

/// `select_and_acquire_slot` 的返回元组：
/// `(id, credentials, from_cooldown_fallback, earliest_cooldown_until, permit)`。
type SlotSelection = (
    u64,
    KiroCredentials,
    bool,
    Option<Instant>,
    Option<OwnedSemaphorePermit>,
);

/// 单个凭据 per-credential 并发 permit 的尝试结果。
enum PermitStatus {
    /// 该凭据未配置 `max_inflight_per_credential`，不限并发。
    Unlimited,
    /// 成功取得 permit（per-request 持有，drop 时归还）。
    Acquired(OwnedSemaphorePermit),
    /// 已配置限制且当前满载（无空位）。
    Full,
}

/// 非阻塞尝试取得某凭据的并发 permit，区分"不限/取到/满载"。
fn acquire_permit_status(entry: &CredentialEntry) -> PermitStatus {
    match entry.permit_semaphore.as_ref() {
        None => PermitStatus::Unlimited,
        Some(sem) => match sem.clone().try_acquire_owned() {
            Ok(permit) => PermitStatus::Acquired(permit),
            Err(_) => PermitStatus::Full,
        },
    }
}

/// acquire-or-degrade：取到则返回 permit，未配置限制或满载都返回 `None`
/// （满载时不阻塞、不跨号跳过——用于 sticky / fallback 这类"只认这一个号"的路径）。
fn try_acquire_permit(entry: &CredentialEntry) -> Option<OwnedSemaphorePermit> {
    match acquire_permit_status(entry) {
        PermitStatus::Acquired(permit) => Some(permit),
        PermitStatus::Unlimited | PermitStatus::Full => None,
    }
}

impl MultiTokenManager {
    /// 选择一个可用凭据并原子地占用 inflight 槽位 + per-credential 并发 permit
    ///
    /// 在持有 entries 锁的同时把 `inflight += 1`，
    /// 这样并发调用会立刻看到该号 inflight 升高，下一个调用自然分发到其他号。
    ///
    /// 调用方拿到结果后，**必须**通过 `release_inflight(id)`（或
    /// `report_success` / `report_failure` 等会自动释放 inflight 的接口）归还 inflight 槽位，
    /// 否则会泄漏。并发 permit 则由返回的 `OwnedSemaphorePermit` 所有权管理：移交给
    /// 本次请求的 `CallContext`，请求结束 drop 时自动归还 semaphore。
    ///
    /// 返回 `(id, credentials, from_cooldown_fallback, earliest_cooldown_until, permit)`：
    /// - `from_cooldown_fallback=true` 表示全员都在 cooldown 中、本次是无奈选了最早
    ///   过期的号"硬试"，调用方在失败上报时不应再延长 cooldown（防雪暴）。
    /// - `earliest_cooldown_until`: 全员 cooldown 时是被选中号的过期时刻；非 fallback
    ///   分支为 `None`。调用方可据此决定是否短暂等待后重新选号（智能等待）。
    /// - `permit`: 该号 `max_inflight_per_credential` 的并发 permit；未配置限制或满载
    ///   降级时为 `None`。
    ///
    /// **并发上限语义**：live 候选池按调度键排序后逐个尝试 `try_acquire_owned`，
    /// 跳过已满的号选下一个真正有空位的；仅当所有 live 候选都满时才退化为"不限并发"
    /// 借用调度键最优的那个。sticky / 全员 cooldown fallback 路径只对单个选中号做
    /// acquire-or-degrade（不跨号跳过，保持其路由意图）。
    ///
    /// **`exclude`（per-request 已试过的凭据）**：软排除——仅当排除后仍 ≥1 候选时才生效，
    /// 否则忽略（即"所有号都试过了"时自动重置，回到正常选号，避免请求直接饿死）。
    /// 用于同一请求的 retry 不要反复打同一个刚失败的号。
    pub(super) fn select_and_acquire_slot(
        &self,
        model: Option<&str>,
        conversation_id: Option<&str>,
        exclude: &HashSet<u64>,
    ) -> Option<SlotSelection> {
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

        // 软排除已试过的凭据：排除后仍有候选才采用,否则保留全集(=自动重置)。
        let candidates: Vec<usize> = if exclude.is_empty() {
            candidates
        } else {
            let filtered: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|&i| !exclude.contains(&entries[i].id))
                .collect();
            if filtered.is_empty() {
                candidates
            } else {
                filtered
            }
        };

        // Sticky session：同一 conversation_id 优先路由到同一凭据
        if let Some(cid) = conversation_id {
            if let Some(sticky_id) = self.sticky_router.get(cid) {
                if let Some(&idx) = candidates.iter().find(|&&i| entries[i].id == sticky_id) {
                    if !is_in_cooldown(&entries[idx], now) {
                        // Sticky 命中且不在 cooldown：只对该号 acquire-or-degrade（不跨号跳过）
                        let permit = try_acquire_permit(&entries[idx]);
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
                            permit,
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

        if all_in_cooldown {
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
            let chosen_idx = *candidates.iter().min_by_key(|&&i| {
                let e = &entries[i];
                (
                    e.last_transient_at_instant,
                    e.cooldown_until.unwrap_or(now),
                    e.inflight,
                    e.success_count,
                    e.credentials.priority,
                    fastrand::u32(..),
                )
            })?;
            // fallback 是"硬试"，只对单号 acquire-or-degrade，不跨号跳过
            let permit = try_acquire_permit(&entries[chosen_idx]);
            let entry = &mut entries[chosen_idx];
            entry.inflight = entry.inflight.saturating_add(1);
            let earliest_until = entry.cooldown_until;
            return Some((
                entry.id,
                entry.credentials.clone(),
                true,
                earliest_until,
                permit,
            ));
        }

        // live 候选池：按调度键排序（best-first），随后逐个尝试取 permit，
        // 跳过已满的号选下一个真正有空位的。
        let mut ordered = live;
        match mode {
            "balanced" => {
                // In-Flight 优先：N 个并发请求自然分散到 N 个号上。
                // 次级键 last_transient_at_instant：None < Some 且老 Instant < 新 Instant，
                // 让"最久没失败的号"优先（避免刚 cooldown 过期立即又被选中）。
                // 末尾追加随机数破平局，防 Vec 第一项被永久选中。
                ordered.sort_by_cached_key(|&i| {
                    let e = &entries[i];
                    (
                        e.inflight,
                        e.last_transient_at_instant,
                        e.success_count,
                        e.credentials.priority,
                        fastrand::u32(..),
                    )
                });
            }
            _ => {
                // priority 模式：先按优先级，同优先级偏好"最久没失败的号"
                ordered.sort_by_cached_key(|&i| {
                    let e = &entries[i];
                    (
                        e.credentials.priority,
                        e.last_transient_at_instant,
                        e.success_count,
                    )
                });
            }
        }

        // 逐个尝试：第一个能取到 permit（或本就不限并发）的号即选中。
        for &idx in &ordered {
            match acquire_permit_status(&entries[idx]) {
                PermitStatus::Unlimited => {
                    let entry = &mut entries[idx];
                    entry.inflight = entry.inflight.saturating_add(1);
                    return Some((entry.id, entry.credentials.clone(), false, None, None));
                }
                PermitStatus::Acquired(permit) => {
                    let entry = &mut entries[idx];
                    entry.inflight = entry.inflight.saturating_add(1);
                    return Some((
                        entry.id,
                        entry.credentials.clone(),
                        false,
                        None,
                        Some(permit),
                    ));
                }
                PermitStatus::Full => continue,
            }
        }

        // 所有 live 候选都满载：退化为"不限并发"，借用调度键最优的那个（ordered[0]）。
        let chosen_idx = *ordered.first()?;
        tracing::debug!(
            "所有 live 凭据 per-credential 并发已满，降级为不限并发借用凭据 #{}",
            entries[chosen_idx].id
        );
        let entry = &mut entries[chosen_idx];
        entry.inflight = entry.inflight.saturating_add(1);
        Some((entry.id, entry.credentials.clone(), false, None, None))
    }

    /// 释放 inflight 槽位（请求结束时调用）。
    ///
    /// per-credential 并发 permit 不在此释放——它由本次请求的 `CallContext` 所有，
    /// `CallContext` drop 时自动归还 semaphore。
    pub fn release_inflight(&self, id: u64) {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            entry.inflight = entry.inflight.saturating_sub(1);
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

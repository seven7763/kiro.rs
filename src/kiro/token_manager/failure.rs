//! 成功/失败/瞬态错误上报与凭据切换

use super::*;

impl MultiTokenManager {
    /// 报告凭据遇到上游瞬态错误（429/408/5xx）
    ///
    /// 与 `report_failure` 不同：**不**累计 `failure_count`、**不**禁用凭据；
    /// 仅累计 `transient_failure_count` 并（视情况）把该凭据放入短期冷却。意图：
    ///
    /// - 单请求 retry 期间不再重复打到刚 429 的号；
    /// - 进入 cooldown 的号不会被永久禁用，到期自动恢复；
    /// - 全部号都被冷却时调度器 fallback 到"最早过期"的号，避免 503 风暴。
    ///
    /// 防雪暴：cooldown 时长加 ±20% jitter，错峰恢复，避免一批号同步进/出冷却。
    /// 防 fallback 死循环：`from_cooldown_fallback=true` 时**只累计计数、不刷新 cooldown**——
    /// 这种调用本来就是无奈选了一个还在冷却中的号"硬试"，再延长它的 cooldown 只会
    /// 让它永远是"最早过期"反复被借用，导致没有号能真正恢复。
    ///
    /// # 参数
    /// - `id`: 凭据 ID
    /// - `kind`: 错误分类（决定默认 cooldown 时长）
    /// - `retry_after`: 上游 `Retry-After` 头解析出的等待时长（如有则优先于默认值，无 jitter）
    /// - `from_cooldown_fallback`: 本次调用是否来自全员冷却的 fallback 路径
    pub fn report_transient_failure(
        &self,
        id: u64,
        kind: TransientFailureKind,
        retry_after: Option<StdDuration>,
        from_cooldown_fallback: bool,
    ) {
        self.report_transient_failure_with_directory(
            id,
            kind,
            retry_after,
            from_cooldown_fallback,
            None,
        );
    }

    /// 上报瞬态失败，并可携带从 suspicious activity 响应中提取的 directory key。
    pub fn report_transient_failure_with_directory(
        &self,
        id: u64,
        kind: TransientFailureKind,
        retry_after: Option<StdDuration>,
        from_cooldown_fallback: bool,
        directory_key: Option<&str>,
    ) {
        // toggle 关闭时退化为旧行为：仅释放 inflight
        if !self.effective_transient_cooldown_enabled() {
            self.release_inflight(id);
            return;
        }

        // 计算基础 cooldown 时长：
        // - OVERAGE 速率窗口通常按 hour/day 计，**不**接受 retry_after 缩短（即使有也以默认值为准）
        // - SuspiciousActivity 为 directory 级别封禁，使用独立长 cooldown（默认 300s），
        //   **不**接受 retry_after 缩短（上游返回的短 Retry-After 不适用于风控场景）
        // - 其余类型 retry_after 优先（夹到 RETRY_AFTER_MAX_CLAMP），否则按 kind 选默认值
        let base_duration = if matches!(kind, TransientFailureKind::OverageRequestLimit) {
            self.effective_overage_request_cooldown()
        } else if matches!(kind, TransientFailureKind::SuspiciousActivity) {
            self.effective_suspicious_activity_cooldown()
        } else if let Some(d) = retry_after {
            d.min(RETRY_AFTER_MAX_CLAMP)
        } else {
            match kind {
                TransientFailureKind::RateLimit => self.effective_rate_limit_cooldown(),
                TransientFailureKind::Timeout | TransientFailureKind::UpstreamError => {
                    self.effective_upstream_error_cooldown()
                }
                TransientFailureKind::OverageRequestLimit
                | TransientFailureKind::SuspiciousActivity => unreachable!(),
            }
        };

        // ±20% jitter 错峰恢复（仅对默认值/配置值；上游显式 Retry-After 不抖动以尊重协议）
        let duration = if retry_after.is_some() {
            base_duration
        } else {
            apply_cooldown_jitter(base_duration)
        };

        let now_instant = Instant::now();
        let cooldown_until = now_instant + duration;
        let now_rfc = Utc::now().to_rfc3339();
        let observed_directory_key = if matches!(kind, TransientFailureKind::SuspiciousActivity) {
            directory_key
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        } else {
            None
        };
        let mut became_dirty = false;

        {
            let mut entries = self.entries.lock();
            if let Some(idx) = entries.iter().position(|e| e.id == id) {
                let effective_directory_key = {
                    let entry = &mut entries[idx];
                    entry.inflight = entry.inflight.saturating_sub(1);
                    entry.transient_failure_count = entry.transient_failure_count.saturating_add(1);
                    entry.last_transient_failure_at = Some(now_rfc);
                    entry.last_transient_at_instant = Some(now_instant);

                    if let Some(key) = &observed_directory_key {
                        entry.directory_key = Some(key.clone());
                    }
                    let known_directory_key = observed_directory_key
                        .clone()
                        .or_else(|| entry.directory_key.clone());

                    if from_cooldown_fallback {
                        // 全员冷却 fallback 路径：不刷新 cooldown，让它按原计划过期。
                        // 否则会把"最早过期"的号永久推到末尾，导致 directory 整体限流时
                        // 没有号能恢复。
                        tracing::warn!(
                            "凭据 #{} 瞬态失败（fallback 路径，不延长冷却；原因 {}，累计 {} 次）",
                            id,
                            kind.as_str(),
                            entry.transient_failure_count
                        );
                    } else {
                        // 只放宽 cooldown，不缩短：避免短的 retry-after 覆盖前面的长冷却
                        let extend = entry
                            .cooldown_until
                            .map(|until| cooldown_until > until)
                            .unwrap_or(true);
                        if extend {
                            entry.cooldown_until = Some(cooldown_until);
                        }
                        entry.cooldown_reason = Some(kind);
                        tracing::warn!(
                            "凭据 #{} 进入冷却 {}s（原因 {}，累计瞬态失败 {} 次）",
                            id,
                            duration.as_secs(),
                            kind.as_str(),
                            entry.transient_failure_count
                        );
                    }

                    known_directory_key
                };

                if matches!(kind, TransientFailureKind::SuspiciousActivity)
                    && !from_cooldown_fallback
                    && let Some(directory_key) = effective_directory_key
                {
                    let mut affected = 1usize;
                    for peer in entries.iter_mut() {
                        if peer.id == id
                            || peer.disabled
                            || peer.directory_key.as_deref() != Some(directory_key.as_str())
                        {
                            continue;
                        }
                        let extend = peer
                            .cooldown_until
                            .map(|until| cooldown_until > until)
                            .unwrap_or(true);
                        if extend {
                            peer.cooldown_until = Some(cooldown_until);
                        }
                        peer.cooldown_reason = Some(kind);
                        affected += 1;
                    }
                    if affected > 1 {
                        tracing::warn!(
                            "directory {} suspicious activity 冷却已应用到 {} 个凭据（{}s）",
                            directory_key,
                            affected,
                            duration.as_secs()
                        );
                    } else {
                        tracing::warn!(
                            "凭据 #{} 记录 suspicious activity directory {}（暂无已知同组凭据）",
                            id,
                            directory_key
                        );
                    }
                } else if matches!(kind, TransientFailureKind::SuspiciousActivity)
                    && observed_directory_key.is_none()
                {
                    tracing::warn!(
                        "凭据 #{} suspicious activity 响应未解析到 directory key，仅冷却当前凭据",
                        id
                    );
                }
                became_dirty = true;
            }
        }

        if became_dirty {
            self.save_stats_debounced();
        }
    }

    /// 报告指定凭据 API 调用成功
    ///
    /// 重置该凭据的失败计数
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    pub fn report_success(&self, id: u64) {
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.success_count += 1;
                entry.last_used_at = Some(Utc::now().to_rfc3339());
                entry.inflight = entry.inflight.saturating_sub(1);
                // 成功调用证明上游对该号已恢复，立即解除冷却
                entry.cooldown_until = None;
                entry.cooldown_reason = None;
                tracing::debug!(
                    "凭据 #{} API 调用成功（累计 {} 次，当前并发 {}）",
                    id,
                    entry.success_count,
                    entry.inflight
                );
            }
        }
        self.save_stats_debounced();
    }

    /// 报告指定凭据 API 调用失败
    ///
    /// 增加失败计数，达到阈值时禁用凭据并切换到优先级最高的可用凭据
    /// 返回是否还有可用凭据可以重试
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    pub fn report_failure(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            // 释放 inflight 槽（请求已结束）
            entry.inflight = entry.inflight.saturating_sub(1);

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.failure_count += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            let failure_count = entry.failure_count;

            tracing::warn!(
                "凭据 #{} API 调用失败（{}/{}）",
                id,
                failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if failure_count >= MAX_FAILURES_PER_CREDENTIAL {
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::TooManyFailures);
                tracing::error!("凭据 #{} 已连续失败 {} 次，已被禁用", id, failure_count);

                // 切换到优先级最高的可用凭据
                if let Some(next) = entries
                    .iter()
                    .filter(|e| !e.disabled)
                    .min_by_key(|e| e.credentials.priority)
                {
                    *current_id = next.id;
                    tracing::info!(
                        "已切换到凭据 #{}（优先级 {}）",
                        next.id,
                        next.credentials.priority
                    );
                } else {
                    tracing::error!("所有凭据均已禁用！");
                }
            }

            entries.iter().any(|e| !e.disabled)
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据额度已用尽
    ///
    /// 用于处理 402 Payment Required 且 reason 为 `MONTHLY_REQUEST_COUNT` 的场景：
    /// - 立即禁用该凭据（不等待连续失败阈值）
    /// - 切换到下一个可用凭据继续重试
    /// - 返回是否还有可用凭据
    pub fn report_quota_exhausted(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            // 释放 inflight 槽（请求已结束）
            entry.inflight = entry.inflight.saturating_sub(1);

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::QuotaExceeded);
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            // 设为阈值，便于在管理面板中直观看到该凭据已不可用
            entry.failure_count = MAX_FAILURES_PER_CREDENTIAL;

            tracing::error!("凭据 #{} 额度已用尽（MONTHLY_REQUEST_COUNT），已被禁用", id);

            // 切换到优先级最高的可用凭据
            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据刷新 Token 失败。
    ///
    /// 连续刷新失败达到阈值后禁用凭据并切换，阈值内保持当前凭据不切换，
    /// 与 API 401/403 的累计失败策略保持一致。
    pub fn report_refresh_failure(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.refresh_failure_count += 1;
            let refresh_failure_count = entry.refresh_failure_count;

            tracing::warn!(
                "凭据 #{} Token 刷新失败（{}/{}）",
                id,
                refresh_failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if refresh_failure_count < MAX_FAILURES_PER_CREDENTIAL {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::TooManyRefreshFailures);

            tracing::error!(
                "凭据 #{} Token 已连续刷新失败 {} 次，已被禁用",
                id,
                refresh_failure_count
            );

            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据的 refreshToken 永久失效（invalid_grant）。
    ///
    /// 立即禁用凭据，不累计、不重试。
    /// 返回是否还有可用凭据。
    pub fn report_refresh_token_invalid(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::InvalidRefreshToken);

            tracing::error!(
                "凭据 #{} refreshToken 已失效 (invalid_grant)，已立即禁用",
                id
            );

            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 切换到优先级最高的可用凭据
    ///
    /// 返回是否成功切换
    pub fn switch_to_next(&self) -> bool {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();

        // 选择优先级最高的未禁用凭据（排除当前凭据）
        if let Some(next) = entries
            .iter()
            .filter(|e| !e.disabled && e.id != *current_id)
            .min_by_key(|e| e.credentials.priority)
        {
            *current_id = next.id;
            tracing::info!(
                "已切换到凭据 #{}（优先级 {}）",
                next.id,
                next.credentials.priority
            );
            true
        } else {
            // 没有其他可用凭据，检查当前凭据是否可用
            entries.iter().any(|e| e.id == *current_id && !e.disabled)
        }
    }
}

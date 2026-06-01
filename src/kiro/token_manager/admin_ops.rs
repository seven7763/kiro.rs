//! Admin CRUD 操作（snapshot / 启停 / 优先级 / 增删 / 余额查询 / 负载均衡模式）

use super::*;

impl MultiTokenManager {
    /// 获取管理器状态快照（用于 Admin API）
    pub fn snapshot(&self) -> ManagerSnapshot {
        let entries = self.entries.lock();
        let current_id = *self.current_id.lock();
        let available = entries.iter().filter(|e| !e.disabled).count();
        let now = Instant::now();

        ManagerSnapshot {
            entries: entries
                .iter()
                .map(|e| {
                    let (proxy_source, effective_proxy_url) =
                        e.credentials
                            .effective_proxy_display(&self.config, self.proxy.as_ref());
                    CredentialEntrySnapshot {
                        id: e.id,
                        priority: e.credentials.priority,
                        disabled: e.disabled,
                        failure_count: e.failure_count,
                        auth_method: if e.credentials.is_api_key_credential() {
                            Some("api_key".to_string())
                        } else {
                            e.credentials.auth_method.as_deref().map(|m| {
                                if m.eq_ignore_ascii_case("builder-id")
                                    || m.eq_ignore_ascii_case("iam")
                                {
                                    "idc".to_string()
                                } else {
                                    m.to_string()
                                }
                            })
                        },
                        has_profile_arn: e.credentials.profile_arn.is_some(),
                        expires_at: if e.credentials.is_api_key_credential() {
                            None // API Key 凭据本地不维护过期时间（服务端策略未知）
                        } else {
                            e.credentials.expires_at.clone()
                        },
                        refresh_token_hash: if e.credentials.is_api_key_credential() {
                            None
                        } else {
                            e.credentials.refresh_token.as_deref().map(sha256_hex)
                        },
                        api_key_hash: if e.credentials.is_api_key_credential() {
                            e.credentials.kiro_api_key.as_deref().map(sha256_hex)
                        } else {
                            None
                        },
                        masked_api_key: if e.credentials.is_api_key_credential() {
                            e.credentials.kiro_api_key.as_deref().map(mask_api_key)
                        } else {
                            None
                        },
                        email: e.credentials.email.clone(),
                        success_count: e.success_count,
                        inflight: e.inflight,
                        last_used_at: e.last_used_at.clone(),
                        has_proxy: effective_proxy_url.is_some(),
                        proxy_url: effective_proxy_url,
                        group: e.credentials.group.clone(),
                        proxy_source: proxy_source.to_string(),
                        refresh_failure_count: e.refresh_failure_count,
                        disabled_reason: e.disabled_reason.map(|r| {
                            match r {
                                DisabledReason::Manual => "Manual",
                                DisabledReason::TooManyFailures => "TooManyFailures",
                                DisabledReason::TooManyRefreshFailures => "TooManyRefreshFailures",
                                DisabledReason::QuotaExceeded => "QuotaExceeded",
                                DisabledReason::InvalidRefreshToken => "InvalidRefreshToken",
                                DisabledReason::InvalidConfig => "InvalidConfig",
                            }
                            .to_string()
                        }),
                        endpoint: e.credentials.endpoint.clone(),
                        transient_failure_count: e.transient_failure_count,
                        last_transient_failure_at: e.last_transient_failure_at.clone(),
                        cooldown_remaining_seconds: e
                            .cooldown_until
                            .and_then(|until| until.checked_duration_since(now))
                            .map(|d| d.as_secs())
                            .unwrap_or(0),
                        cooldown_reason: if e
                            .cooldown_until
                            .map(|until| until > now)
                            .unwrap_or(false)
                        {
                            e.cooldown_reason.map(|r| r.as_str().to_string())
                        } else {
                            None
                        },
                        directory_key: e.directory_key.clone(),
                    }
                })
                .collect(),
            current_id,
            total: entries.len(),
            available,
        }
    }

    /// 设置凭据禁用状态（Admin API）
    pub fn set_disabled(&self, id: u64, disabled: bool) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.disabled = disabled;
            if !disabled {
                // 启用时重置失败计数与冷却（管理员手动启用即清除一切惩罚状态）
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.disabled_reason = None;
                entry.cooldown_until = None;
                entry.cooldown_reason = None;
            } else {
                entry.disabled_reason = Some(DisabledReason::Manual);
            }
        }
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据优先级（Admin API）
    ///
    /// 修改优先级后会立即按新优先级重新选择当前凭据。
    /// 即使持久化失败，内存中的优先级和当前凭据选择也会生效。
    pub fn set_priority(&self, id: u64, priority: u32) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.priority = priority;
        }
        // 立即按新优先级重新选择当前凭据（无论持久化是否成功）
        self.select_highest_priority();
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据分组（Admin API）。`None` 表示移出分组。
    pub fn set_group(&self, id: u64, group: Option<String>) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.group = group
                .map(|g| g.trim().to_string())
                .filter(|g| !g.is_empty());
        }
        self.persist_credentials()?;
        Ok(())
    }

    /// 重置凭据失败计数并重新启用（Admin API）
    pub fn reset_and_enable(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            if entry.disabled_reason == Some(DisabledReason::InvalidConfig) {
                anyhow::bail!("凭据 #{} 因配置无效被禁用，请修正配置后重启服务", id);
            }
            entry.failure_count = 0;
            entry.refresh_failure_count = 0;
            entry.disabled = false;
            entry.disabled_reason = None;
            entry.cooldown_until = None;
            entry.cooldown_reason = None;
        }
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 获取指定凭据的使用额度（Admin API）
    pub async fn get_usage_limits_for(&self, id: u64) -> anyhow::Result<UsageLimitsResponse> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // API Key 凭据直接使用 kiro_api_key，无需刷新
        let token = if credentials.is_api_key_credential() {
            credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?
        } else {
            // 检查是否需要刷新 token
            let needs_refresh =
                is_token_expired(&credentials) || is_token_expiring_soon(&credentials);

            if needs_refresh {
                let lock = self.refresh_lock_for(id);
                let _guard = lock.lock().await;
                let current_creds = {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.credentials.clone())
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
                };

                if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                    let effective_proxy = self.effective_proxy_for(&current_creds);
                    let new_creds =
                        refresh_token(&current_creds, &self.config, effective_proxy.as_ref())
                            .await?;
                    {
                        let mut entries = self.entries.lock();
                        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                            entry.credentials = new_creds.clone();
                        }
                    }
                    // 持久化失败只记录警告，不影响本次请求
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                    }
                    new_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
                } else {
                    current_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
                }
            } else {
                credentials
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        };

        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        let effective_proxy = self.effective_proxy_for(&credentials);
        let usage_limits =
            get_usage_limits(&credentials, &self.config, &token, effective_proxy.as_ref()).await?;

        // 更新订阅等级到凭据（仅在发生变化时持久化）
        if let Some(subscription_title) = usage_limits.subscription_title() {
            let changed = {
                let mut entries = self.entries.lock();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    let old_title = entry.credentials.subscription_title.clone();
                    if old_title.as_deref() != Some(subscription_title) {
                        entry.credentials.subscription_title = Some(subscription_title.to_string());
                        tracing::info!(
                            "凭据 #{} 订阅等级已更新: {:?} -> {}",
                            id,
                            old_title,
                            subscription_title
                        );
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if changed {
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("订阅等级更新后持久化失败（不影响本次请求）: {}", e);
                }
            }
        }

        Ok(usage_limits)
    }

    /// 添加新凭据（Admin API）
    ///
    /// # 流程
    /// 1. 验证凭据基本字段（API Key: kiroApiKey 不为空; OAuth: refreshToken 不为空）
    /// 2. 基于 kiroApiKey 或 refreshToken 的 SHA-256 哈希检测重复
    /// 3. OAuth: 尝试刷新 Token 验证凭据有效性; API Key: 跳过
    /// 4. 分配新 ID（当前最大 ID + 1）
    /// 5. 添加到 entries 列表
    /// 6. 持久化到配置文件
    ///
    /// # 返回
    /// - `Ok(u64)` - 新凭据 ID
    /// - `Err(_)` - 验证失败或添加失败
    pub async fn add_credential(&self, new_cred: KiroCredentials) -> anyhow::Result<u64> {
        // 1. 基本验证
        if new_cred.is_api_key_credential() {
            let api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            if api_key.is_empty() {
                anyhow::bail!("kiroApiKey 为空");
            }
        } else {
            validate_refresh_token(&new_cred)?;
        }

        // 2. 基于哈希检测重复
        if new_cred.is_api_key_credential() {
            let new_api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 kiroApiKey"))?;
            let new_api_key_hash = sha256_hex(new_api_key);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .kiro_api_key
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_api_key_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（kiroApiKey 重复）");
            }
        } else {
            let new_refresh_token = new_cred
                .refresh_token
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;
            let new_refresh_token_hash = sha256_hex(new_refresh_token);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .refresh_token
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_refresh_token_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（refreshToken 重复）");
            }
        }

        // 3. 验证凭据有效性（API Key 无需网络刷新）
        let mut validated_cred = if new_cred.is_api_key_credential() {
            new_cred.clone()
        } else {
            let effective_proxy = self.effective_proxy_for(&new_cred);
            refresh_token(&new_cred, &self.config, effective_proxy.as_ref()).await?
        };

        // 4. 分配新 ID
        let new_id = {
            let entries = self.entries.lock();
            entries.iter().map(|e| e.id).max().unwrap_or(0) + 1
        };

        // 5. 设置 ID 并保留用户输入的元数据
        validated_cred.id = Some(new_id);
        validated_cred.priority = new_cred.priority;
        validated_cred.auth_method = new_cred.auth_method.map(|m| {
            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam") {
                "idc".to_string()
            } else {
                m
            }
        });
        validated_cred.client_id = new_cred.client_id;
        validated_cred.client_secret = new_cred.client_secret;
        validated_cred.region = new_cred.region;
        validated_cred.auth_region = new_cred.auth_region;
        validated_cred.api_region = new_cred.api_region;
        validated_cred.machine_id = new_cred.machine_id;
        validated_cred.email = new_cred.email;
        validated_cred.proxy_url = new_cred.proxy_url;
        validated_cred.proxy_username = new_cred.proxy_username;
        validated_cred.proxy_password = new_cred.proxy_password;
        validated_cred.group = new_cred.group;
        validated_cred.kiro_api_key = new_cred.kiro_api_key;

        {
            let mut entries = self.entries.lock();
            // 复用已有凭据的 per-credential semaphore（所有凭据共享同一配置的 semaphore）
            let per_cred_sem = entries.iter().find_map(|e| e.permit_semaphore.clone());
            entries.push(CredentialEntry {
                id: new_id,
                credentials: validated_cred,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled: false,
                disabled_reason: None,
                success_count: 0,
                last_used_at: None,
                inflight: 0,
                transient_failure_count: 0,
                last_transient_failure_at: None,
                last_transient_at_instant: None,
                cooldown_until: None,
                cooldown_reason: None,
                directory_key: None,
                permit_semaphore: per_cred_sem,
            });
        }

        // 6. 持久化
        self.persist_credentials()?;

        tracing::info!("成功添加凭据 #{}", new_id);
        Ok(new_id)
    }

    /// 删除凭据（Admin API）
    ///
    /// # 前置条件
    /// - 凭据必须已禁用（disabled = true）
    ///
    /// # 行为
    /// 1. 验证凭据存在
    /// 2. 验证凭据已禁用
    /// 3. 从 entries 移除
    /// 4. 如果删除的是当前凭据，切换到优先级最高的可用凭据
    /// 5. 如果删除后没有凭据，将 current_id 重置为 0
    /// 6. 持久化到文件
    ///
    /// # 返回
    /// - `Ok(())` - 删除成功
    /// - `Err(_)` - 凭据不存在、未禁用或持久化失败
    pub fn delete_credential(&self, id: u64) -> anyhow::Result<()> {
        let was_current = {
            let mut entries = self.entries.lock();

            // 查找凭据
            let entry = entries
                .iter()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;

            // 检查是否已禁用
            if !entry.disabled {
                anyhow::bail!("只能删除已禁用的凭据（请先禁用凭据 #{}）", id);
            }

            // 记录是否是当前凭据
            let current_id = *self.current_id.lock();
            let was_current = current_id == id;

            // 删除凭据
            entries.retain(|e| e.id != id);

            was_current
        };

        // 如果删除的是当前凭据，切换到优先级最高的可用凭据
        if was_current {
            self.select_highest_priority();
        }

        // 如果删除后没有任何凭据，将 current_id 重置为 0（与初始化行为保持一致）
        {
            let entries = self.entries.lock();
            if entries.is_empty() {
                let mut current_id = self.current_id.lock();
                *current_id = 0;
                tracing::info!("所有凭据已删除，current_id 已重置为 0");
            }
        }

        // 持久化更改
        self.persist_credentials()?;

        // 立即回写统计数据，清除已删除凭据的残留条目
        self.save_stats();

        tracing::info!("已删除凭据 #{}", id);
        Ok(())
    }

    /// 获取负载均衡模式（Admin API）
    pub fn get_load_balancing_mode(&self) -> String {
        self.load_balancing_mode.lock().clone()
    }

    /// 设置负载均衡模式（Admin API）
    pub fn set_load_balancing_mode(&self, mode: String) -> anyhow::Result<()> {
        // 验证模式值
        if mode != "priority" && mode != "balanced" {
            anyhow::bail!("无效的负载均衡模式: {}", mode);
        }

        let previous_mode = self.get_load_balancing_mode();
        if previous_mode == mode {
            return Ok(());
        }

        *self.load_balancing_mode.lock() = mode.clone();

        if let Err(err) = self.persist_load_balancing_mode(&mode) {
            *self.load_balancing_mode.lock() = previous_mode;
            return Err(err);
        }

        tracing::info!("负载均衡模式已设置为: {}", mode);
        Ok(())
    }
}

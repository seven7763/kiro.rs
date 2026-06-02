//! 凭据与统计的持久化（stats/credentials/负载均衡模式 原子写）

use super::*;

impl MultiTokenManager {
    /// 将凭据列表回写到源文件
    ///
    /// 仅在以下条件满足时回写：
    /// - 源文件是多凭据格式（数组）
    /// - credentials_path 已设置
    ///
    /// # Returns
    /// - `Ok(true)` - 成功写入文件
    /// - `Ok(false)` - 跳过写入（非多凭据格式或无路径配置）
    /// - `Err(_)` - 写入失败
    pub(super) fn persist_credentials(&self) -> anyhow::Result<bool> {
        use anyhow::Context;

        // 仅多凭据格式才回写
        if !self.is_multiple_format {
            return Ok(false);
        }

        let path = match &self.credentials_path {
            Some(p) => p,
            None => return Ok(false),
        };

        // 收集所有凭据
        let credentials: Vec<KiroCredentials> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    let mut cred = e.credentials.clone();
                    cred.canonicalize_auth_method();
                    // 同步 disabled 状态到凭据对象
                    cred.disabled = e.disabled;
                    cred
                })
                .collect()
        };

        // 序列化为 pretty JSON
        let json = serde_json::to_string_pretty(&credentials).context("序列化凭据失败")?;

        // 原子写入（tmp + rename），防进程中段被 kill 时 credentials.json 半写损坏。
        // 用 _secure 变体：Unix 上自动 chmod 0o600，防同主机其他用户/服务读到
        // refresh_token / access_token / api_key 等敏感字段。
        // 在 Tokio runtime 内使用 block_in_place 避免阻塞 worker。
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| {
                crate::common::io::atomic_write_string_secure(path, &json)
            })
            .with_context(|| format!("回写凭据文件失败: {:?}", path))?;
        } else {
            crate::common::io::atomic_write_string_secure(path, &json)
                .with_context(|| format!("回写凭据文件失败: {:?}", path))?;
        }

        tracing::debug!("已回写凭据到文件: {:?}", path);
        Ok(true)
    }

    /// 统计数据文件路径
    fn stats_path(&self) -> Option<PathBuf> {
        self.cache_dir().map(|d| d.join("kiro_stats.json"))
    }

    /// 从磁盘加载统计数据并应用到当前条目
    pub(super) fn load_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return, // 首次运行时文件不存在
        };

        let stats: HashMap<String, StatsEntry> = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("解析统计缓存失败，将忽略: {}", e);
                return;
            }
        };

        let mut entries = self.entries.lock();
        let now_instant = Instant::now();
        let now_wall = chrono::Utc::now();
        let mut restored_cooldowns = 0usize;
        for entry in entries.iter_mut() {
            if let Some(s) = stats.get(&entry.id.to_string()) {
                entry.success_count = s.success_count;
                entry.last_used_at = s.last_used_at.clone();
                entry.transient_failure_count = s.transient_failure_count;
                entry.last_transient_failure_at = s.last_transient_failure_at.clone();
                // 恢复 cooldown：仅当落盘的到期墙钟仍在未来。把"墙钟剩余"换算回 Instant：
                // cooldown_until = now_instant + (到期墙钟 - now_wall)。过期/解析失败则忽略。
                if let Some(rfc) = s.cooldown_until_rfc3339.as_deref() {
                    if let Ok(until_wall) = chrono::DateTime::parse_from_rfc3339(rfc) {
                        let until_utc = until_wall.with_timezone(&chrono::Utc);
                        if until_utc > now_wall {
                            if let Ok(remaining) = (until_utc - now_wall).to_std() {
                                entry.cooldown_until = Some(now_instant + remaining);
                                entry.cooldown_reason = s.cooldown_reason;
                                restored_cooldowns += 1;
                            }
                        }
                    }
                }
            }
        }
        *self.last_stats_save_at.lock() = Some(Instant::now());
        self.stats_dirty.store(false, Ordering::Relaxed);
        tracing::info!(
            "已从缓存加载 {} 条统计数据（恢复 {} 个未过期 cooldown）",
            stats.len(),
            restored_cooldowns
        );
    }

    /// 将当前统计数据持久化到磁盘
    pub(super) fn save_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let stats: HashMap<String, StatsEntry> = {
            let now_instant = Instant::now();
            let now_wall = chrono::Utc::now();
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    // cooldown_until 是 Instant（进程内），落盘前换算成墙钟绝对时间：
                    // 剩余时长 = cooldown_until - now_instant，到期墙钟 = now_wall + 剩余。
                    // 已过期（<= now）的不落盘。
                    let cooldown_until_rfc3339 = e.cooldown_until.and_then(|until| {
                        let remaining = until.saturating_duration_since(now_instant);
                        if remaining.is_zero() {
                            None
                        } else {
                            chrono::Duration::from_std(remaining)
                                .ok()
                                .map(|d| (now_wall + d).to_rfc3339())
                        }
                    });
                    (
                        e.id.to_string(),
                        StatsEntry {
                            success_count: e.success_count,
                            last_used_at: e.last_used_at.clone(),
                            transient_failure_count: e.transient_failure_count,
                            last_transient_failure_at: e.last_transient_failure_at.clone(),
                            cooldown_reason: cooldown_until_rfc3339.as_ref().and(e.cooldown_reason),
                            cooldown_until_rfc3339,
                        },
                    )
                })
                .collect()
        };

        match serde_json::to_string_pretty(&stats) {
            Ok(json) => {
                if let Err(e) = crate::common::io::atomic_write_string(&path, &json) {
                    tracing::warn!("保存统计缓存失败: {}", e);
                } else {
                    *self.last_stats_save_at.lock() = Some(Instant::now());
                    self.stats_dirty.store(false, Ordering::Relaxed);
                }
            }
            Err(e) => tracing::warn!("序列化统计数据失败: {}", e),
        }
    }

    /// 标记统计数据已更新，并按 debounce 策略决定是否立即落盘
    pub(super) fn save_stats_debounced(&self) {
        self.stats_dirty.store(true, Ordering::Relaxed);

        let should_flush = {
            let last = *self.last_stats_save_at.lock();
            match last {
                Some(last_saved_at) => last_saved_at.elapsed() >= STATS_SAVE_DEBOUNCE,
                None => true,
            }
        };

        if should_flush {
            self.save_stats();
        }
    }

    pub(super) fn persist_load_balancing_mode(&self, mode: &str) -> anyhow::Result<()> {
        use anyhow::Context;

        let config_path = match self.config.config_path() {
            Some(path) => path.to_path_buf(),
            None => {
                tracing::warn!("配置文件路径未知，负载均衡模式仅在当前进程生效: {}", mode);
                return Ok(());
            }
        };

        let mut config = Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;
        config.load_balancing_mode = mode.to_string();
        config
            .save()
            .with_context(|| format!("持久化负载均衡模式失败: {}", config_path.display()))?;

        Ok(())
    }

    /// 将凭据分组列表回写到 config.json。
    ///
    /// 沿用 [`Self::persist_load_balancing_mode`] 的「reload config → 改字段 → save」
    /// 原子写范式：只覆盖 `credential_groups` 一个字段，避免与其他运行时回写互相踩。
    /// 配置文件路径未知时仅在当前进程生效（与负载均衡模式一致）。
    pub(super) fn persist_credential_groups(
        &self,
        groups: &[CredentialGroupConfig],
    ) -> anyhow::Result<()> {
        use anyhow::Context;

        let config_path = match self.config.config_path() {
            Some(path) => path.to_path_buf(),
            None => {
                tracing::warn!("配置文件路径未知，凭据分组仅在当前进程生效");
                return Ok(());
            }
        };

        let mut config = Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;
        config.credential_groups = groups.to_vec();
        config
            .save()
            .with_context(|| format!("持久化凭据分组失败: {}", config_path.display()))?;

        Ok(())
    }
}

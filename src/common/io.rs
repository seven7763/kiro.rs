//! 文件 I/O 工具
//!
//! 主要提供原子写入实现，避免进程中段被 kill 时配置/凭据文件半写损坏。
//! 对凭据等敏感文件还提供 `_secure` 变体，自动 chmod 0o600 防同主机权限泄露。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 敏感文件 Unix 权限：仅 owner 可读写
#[cfg(unix)]
const SECURE_FILE_MODE: u32 = 0o600;

/// 原子地把字符串写入文件
///
/// 实现：
/// 1. 写入同目录下临时文件 `<target>.tmp.<pid>.<random>`
/// 2. `flush + sync_all` 确保数据落盘
/// 3. `rename(tmp, target)` —— POSIX 上 rename 是原子的
///
/// 进程在 step 1/2 中段被 kill：目标文件**完全不受影响**，只留一个孤立 `.tmp` 文件
/// （可被下次启动或运维清理）。
///
/// 进程在 step 3 中段被 kill：rename 是原子操作，要么成功要么失败，目标文件
/// 不会处于半写状态。
///
/// # 平台说明
/// - macOS / Linux：原子（POSIX rename 语义）
/// - Windows：在文件系统支持的前提下原子；NTFS 默认支持
///
/// # Errors
/// 写临时文件失败、flush 失败、rename 失败时返回原始 io::Error 上下文。
pub fn atomic_write_string<P: AsRef<Path>>(path: P, content: &str) -> std::io::Result<()> {
    let path = path.as_ref();

    let tmp_path = make_tmp_path(path);

    // 写入临时文件
    {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
        // sync_all 确保数据 + 元数据都落盘
        file.sync_all()?;
    }

    // 原子替换目标文件
    match std::fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // rename 失败：清理孤立临时文件，避免堆积
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

/// 原子地把字符串写入文件，并设置敏感文件权限
///
/// 与 [`atomic_write_string`] 行为一致，额外在 rename **前**对临时文件
/// `chmod 0o600`（Unix 平台），让目标文件落盘后立即只对 owner 可读写。
///
/// 用于 `credentials.json` 等含有 refresh_token / access_token 的敏感文件，
/// 防止同主机其他用户/服务/容器直接读取。
///
/// # 平台说明
/// - Unix（macOS / Linux）：`set_permissions(0o600)`
/// - Windows / 非 Unix：跳过权限设置（ACL 模型不同），写入仍是原子的
///
/// # 错误处理
/// `set_permissions` 失败仅 warn 不阻塞写入：某些 NFS / CIFS / overlayfs
/// 不支持 chmod，但数据完整性必须优先保证。
pub fn atomic_write_string_secure<P: AsRef<Path>>(
    path: P,
    content: &str,
) -> std::io::Result<()> {
    let path = path.as_ref();
    let tmp_path = make_tmp_path(path);

    // 写入临时文件
    {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
        file.sync_all()?;
    }

    // 设置权限：rename 前完成，避免目标文件出现"已替换但权限尚未收紧"的窗口
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(SECURE_FILE_MODE);
        if let Err(e) = std::fs::set_permissions(&tmp_path, perms) {
            tracing::warn!(
                "为 {} 设置 0o600 权限失败（继续写入）: {}",
                tmp_path.display(),
                e
            );
        }
    }

    // 原子替换
    match std::fs::rename(&tmp_path, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

/// 为目标路径生成同目录临时文件路径
///
/// 形如 `<target>.tmp.<pid>.<random>`，保证：
/// - 同目录（rename 必须同一文件系统才原子）
/// - PID + 随机数防多实例并发冲突
fn make_tmp_path(target: &Path) -> PathBuf {
    let pid = std::process::id();
    // 简单的 nanosecond 随机数，避免引入 rand 依赖
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);

    let mut tmp = target.to_path_buf();
    let suffix = format!(
        "{}.tmp.{}.{}",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file"),
        pid,
        nanos
    );
    tmp.set_file_name(suffix);
    tmp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_target() -> PathBuf {
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        dir.join(format!("atomic-write-test-{}-{}.json", pid, nanos))
    }

    #[test]
    fn atomic_write_creates_and_overwrites() {
        let target = tmp_target();

        // 首次写入
        atomic_write_string(&target, "hello").expect("first write");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");

        // 覆盖
        atomic_write_string(&target, "world!").expect("overwrite");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "world!");

        // 清理
        std::fs::remove_file(&target).ok();
    }

    #[test]
    fn atomic_write_does_not_leave_tmp_files_on_success() {
        let target = tmp_target();
        let dir = target.parent().unwrap();
        let target_name = target.file_name().unwrap().to_string_lossy().to_string();

        atomic_write_string(&target, "content").expect("write");

        // 检查目录里不应残留任何 `<target>.tmp.*`
        let leftover_tmp = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with(&target_name) && name.contains(".tmp.")
            })
            .count();
        assert_eq!(leftover_tmp, 0, "成功路径不应残留临时文件");

        std::fs::remove_file(&target).ok();
    }

    #[test]
    fn atomic_write_handles_empty_content() {
        let target = tmp_target();
        atomic_write_string(&target, "").expect("empty write");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
        std::fs::remove_file(&target).ok();
    }

    /// secure 写入：内容正确 + 文件存在
    ///
    /// 该测试不依赖 unix（用于 Windows CI 也能跑），权限校验单独走 unix 分支
    #[test]
    fn secure_write_creates_file_with_content() {
        let target = tmp_target();
        atomic_write_string_secure(&target, "secret-data").expect("secure write");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "secret-data",
            "内容应正确写入"
        );
        std::fs::remove_file(&target).ok();
    }

    /// secure 写入后 Unix 权限末 9 位 = 0o600
    #[cfg(unix)]
    #[test]
    fn secure_write_sets_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let target = tmp_target();
        atomic_write_string_secure(&target, "credential-token-here").expect("secure write");

        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        // mode 高位含文件类型 (S_IFREG=0o100000)，只比较低 9 位（rwx）
        assert_eq!(
            mode & 0o777,
            0o600,
            "secure 写入应设权限为 0o600，实际 {:o}",
            mode & 0o777
        );

        std::fs::remove_file(&target).ok();
    }

    /// 已存在的 0o644 文件被 secure 覆盖时应降权到 0o600
    ///
    /// 这个 case 验证了我们对 token_manager.rs 升级的关键场景：
    /// 老版本写出的 credentials.json (0o644) 在升级到含 secure 写入的版本后，
    /// 第一次 token refresh 应自动收紧权限
    #[cfg(unix)]
    #[test]
    fn secure_write_downgrades_existing_644() {
        use std::os::unix::fs::PermissionsExt;

        let target = tmp_target();

        // 模拟老版本：先用普通 atomic_write 写入（默认 0o644 in temp_dir）
        atomic_write_string(&target, "old-content").expect("first write");
        let old_perms = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(&target, old_perms).expect("force 644");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644,
            "前置条件：文件应为 0o644"
        );

        // secure 覆盖
        atomic_write_string_secure(&target, "new-secret").expect("secure overwrite");

        // 验证：内容已更新 + 权限收紧
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new-secret");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600,
            "secure 覆盖应把 0o644 降为 0o600"
        );

        std::fs::remove_file(&target).ok();
    }
}

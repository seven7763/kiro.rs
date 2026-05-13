//! 文件 I/O 工具
//!
//! 主要提供原子写入实现，避免进程中段被 kill 时配置/凭据文件半写损坏。

use std::io::Write;
use std::path::{Path, PathBuf};

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
}

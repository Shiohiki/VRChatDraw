//! 绘制历史与失败记录：每次绘制结束后追加一条 JSON 记录到用户数据目录的 draw_history.json，
//! 保存完成状态、停止原因与关键参数，方便复现问题。文件损坏时静默跳过（仅日志）。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

/// 串行化"读旧档 + 追加 + 原子重写"：快速连续绘制时两个结束线程可能并发写，
/// 不加锁会互相覆盖导致记录丢失。
static HISTORY_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// 结束时间戳（Unix 纳秒）
    pub ts: u128,
    /// 图片来源（文件名或"剪贴板图片"）
    pub image: String,
    /// 图片尺寸（宽×高）
    pub image_size: String,
    pub strokes: usize,
    pub points: usize,
    /// 预计绘制耗时（秒，按当前参数估算）
    pub estimate_seconds: f64,
    /// 绘制结果（DrawResult 名称）
    pub result: String,
    /// 是否为断点续画
    pub resumed: bool,
    /// 输入环境探测结果（Relative/DesktopAbsolute/Undetermined）
    pub probe_mode: String,
    /// 探测备注（探测失败原因等）
    pub probe_note: String,
}

const MAX_ENTRIES: usize = 100;
const MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;

pub fn history_path() -> PathBuf {
    crate::storage::data_path("draw_history.json")
}

/// 追加一条历史记录（原子写：tmp + rename，失败仅 eprintln，不影响主流程）
pub fn append(entry: HistoryEntry) {
    let _guard = HISTORY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (path, mut entries) = match crate::storage::read_preferred("draw_history.json") {
        Ok(None) => (history_path(), Vec::new()),
        Ok(Some((source, bytes))) => {
            if bytes.len() > MAX_HISTORY_BYTES {
                let backup = crate::storage::preserve_corrupt(&source);
                if let Err(error) = backup {
                    eprintln!("历史记录过大且无法备份 {source:#?}: {error}");
                    return;
                }
                (history_path(), Vec::new())
            } else {
                match serde_json::from_slice::<Vec<HistoryEntry>>(&bytes) {
                    Ok(entries) => (history_path(), entries),
                    Err(error) => {
                        if let Err(backup_error) = crate::storage::preserve_corrupt(&source) {
                            eprintln!(
                                "历史记录解析失败且无法备份 {source:#?}: {error}; {backup_error}"
                            );
                            return;
                        }
                        (history_path(), Vec::new())
                    }
                }
            }
        }
        Err(error) => {
            eprintln!("历史记录读取失败：{error}");
            return;
        }
    };
    entries.push(entry);
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
    if let Err(e) = write_entries(&path, &entries) {
        eprintln!("历史记录写入失败：{e}");
    }
}

fn write_entries(path: &std::path::Path, entries: &[HistoryEntry]) -> Result<(), String> {
    let json = serde_json::to_string_pretty(entries).map_err(|e| format!("序列化失败：{e}"))?;
    crate::storage::atomic_write(path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_write_and_append_round_trip() {
        let path = std::env::temp_dir().join("vrc_history_test.json");
        let _ = std::fs::remove_file(&path);

        let entry = |result: &str| HistoryEntry {
            ts: 1,
            image: "a.png".to_string(),
            image_size: "100x100".to_string(),
            strokes: 3,
            points: 12,
            estimate_seconds: 4.2,
            result: result.to_string(),
            resumed: false,
            probe_mode: "Relative".to_string(),
            probe_note: String::new(),
        };
        write_entries(&path, &[entry("Completed")]).expect("首次写入应成功");
        // 第二次写入模拟 append 的"读旧档 + 追加 + 重写"流程
        let mut entries: Vec<HistoryEntry> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        entries.push(entry("Cancelled"));
        write_entries(&path, &entries).expect("追加写入应成功");

        let parsed: Vec<HistoryEntry> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].result, "Completed");
        assert_eq!(parsed[1].result, "Cancelled");
        // 原子替换不应留下临时文件
        assert!(!std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .any(|e| e
                .file_name()
                .to_string_lossy()
                .starts_with("vrc_history_test.json.tmp.")));
        let _ = std::fs::remove_file(&path);
    }
}

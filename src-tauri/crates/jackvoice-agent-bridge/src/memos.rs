//! 读取 JackVoice 待办事项。
//!
//! 只读：不会新增、勾选或删除待办。写操作留在应用里做，避免 sidecar
//! 和桌面端同时改同一个文件。

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

const MEMOS_FILE: &str = "memos.json";
const SOURCE_VOICE: &str = "voice";

fn default_source() -> String {
    SOURCE_VOICE.into()
}

/// 一条待办。字段与应用侧 `MemoItem` 对齐；旧数据没有 source，按语音收入处理。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoEntry {
    pub id: String,
    pub text: String,
    pub created_at_ms: i64,
    #[serde(default)]
    pub completed: bool,
    /// voice（听写置顶收入）/ manual（面板手打）/ clipboard（剪贴板收入）。
    #[serde(default = "default_source")]
    pub source: String,
}

#[derive(Debug, Default, Deserialize)]
struct MemosFile {
    #[serde(default)]
    memos: Vec<MemoEntry>,
}

/// 文件缺失或损坏时返回空列表：读不到待办不该让整个 sidecar 起不来。
pub fn load(dir: &Path) -> Vec<MemoEntry> {
    fs::read_to_string(dir.join(MEMOS_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str::<MemosFile>(&raw).ok())
        .map(|file| file.memos)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 与 tools.rs 同理：并行测试用计数器区分目录，不用时间戳。
    fn test_dir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "jackvoice-bridge-memos-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = test_dir();
        assert!(load(&dir).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_entries_without_source_read_as_voice() {
        let dir = test_dir();
        fs::write(
            dir.join(MEMOS_FILE),
            r#"{"memos":[{"id":"m-1","text":"旧待办","createdAtMs":1000,"completed":false}]}"#,
        )
        .unwrap();
        let memos = load(&dir);
        assert_eq!(memos.len(), 1);
        assert_eq!(memos[0].source, SOURCE_VOICE);
        assert!(!memos[0].completed);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn broken_file_reads_as_empty_instead_of_failing() {
        let dir = test_dir();
        fs::write(dir.join(MEMOS_FILE), "{ 这不是 JSON").unwrap();
        assert!(load(&dir).is_empty());
        fs::remove_dir_all(dir).unwrap();
    }
}

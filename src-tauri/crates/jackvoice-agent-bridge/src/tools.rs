use crate::hotwords::{
    apply_replacements_detailed, load, load_replacements, sanitize_replacements, ReplacementRule,
};
use crate::memos::{load as load_memos, MemoEntry};
use crate::paths::shared_data_dir;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const BRIDGE_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_LIMIT: usize = 5000;

#[derive(Debug, Clone)]
pub struct AgentTools {
    data_dir: PathBuf,
}

impl AgentTools {
    pub fn discover() -> Result<Self, String> {
        Ok(Self::new(shared_data_dir()?))
    }

    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match name {
            "get_status" => self.get_status(),
            "get_glossary" => self.get_glossary(&arguments),
            "get_hotwords" => self.get_hotwords(&arguments),
            "get_replacements" => self.get_replacements(&arguments),
            "apply_replacements" => self.apply_replacements(&arguments),
            "get_memos" => self.get_memos(&arguments),
            _ => Err(format!("未知的 JackVoice MCP 工具：{name}")),
        }
    }

    fn get_status(&self) -> Result<Value, String> {
        let hotwords = load(&self.data_dir);
        let replacements = sanitize_replacements(&load_replacements(&self.data_dir));
        let memos = load_memos(&self.data_dir);
        Ok(json!({
            "bridgeVersion": BRIDGE_VERSION,
            "storeAvailable": self.data_dir.is_dir(),
            "dataDir": self.data_dir,
            "hotwordCount": hotwords.len(),
            "replacementCount": replacements.len(),
            "memoCount": memos.iter().filter(|memo| !memo.completed).count(),
            "capabilities": {
                "readHotwords": true,
                "readReplacements": true,
                "applyReplacements": true,
                "readMemos": true,
                "writeGlossary": false,
                "writeMemos": false,
                "readHistory": false,
                "readRecordings": false
            }
        }))
    }

    /// 默认只给未完成待办：Agent 问「我还有什么没做」时，已完成项只是噪音。
    fn get_memos(&self, arguments: &Value) -> Result<Value, String> {
        let query = optional_query(arguments)?;
        let limit = optional_limit(arguments)?;
        let include_completed = arguments
            .get("includeCompleted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let all = load_memos(&self.data_dir);
        let pending = all.iter().filter(|memo| !memo.completed).count();
        let memos = all
            .into_iter()
            .filter(|memo| include_completed || !memo.completed)
            .filter(|memo| matches_query(&memo.text, query))
            .take(limit)
            .collect::<Vec<MemoEntry>>();
        Ok(json!({
            "memos": memos,
            "count": memos.len(),
            "pendingCount": pending,
            "includeCompleted": include_completed,
            "query": query
        }))
    }

    fn get_glossary(&self, arguments: &Value) -> Result<Value, String> {
        let query = optional_query(arguments)?;
        let limit = optional_limit(arguments)?;
        let hotwords = filter_hotwords(&load(&self.data_dir), query, limit);
        let replacements = filter_replacements(
            &sanitize_replacements(&load_replacements(&self.data_dir)),
            query,
            limit,
        );
        let canonical_terms = canonical_terms(&hotwords, &replacements);
        Ok(json!({
            "hotwords": hotwords,
            "replacements": replacements,
            "canonicalTerms": canonical_terms,
            "hotwordCount": hotwords.len(),
            "replacementCount": replacements.len(),
            "query": query,
        }))
    }

    fn get_hotwords(&self, arguments: &Value) -> Result<Value, String> {
        let query = optional_query(arguments)?;
        let limit = optional_limit(arguments)?;
        let words = filter_hotwords(&load(&self.data_dir), query, limit);
        Ok(json!({
            "hotwords": words,
            "count": words.len(),
            "query": query
        }))
    }

    fn get_replacements(&self, arguments: &Value) -> Result<Value, String> {
        let query = optional_query(arguments)?;
        let limit = optional_limit(arguments)?;
        let rules = filter_replacements(
            &sanitize_replacements(&load_replacements(&self.data_dir)),
            query,
            limit,
        );
        Ok(json!({
            "replacements": rules,
            "count": rules.len(),
            "query": query
        }))
    }

    fn apply_replacements(&self, arguments: &Value) -> Result<Value, String> {
        let text = arguments
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| "apply_replacements 需要 text。".to_string())?;
        let rules = sanitize_replacements(&load_replacements(&self.data_dir))
            .into_iter()
            .map(|rule| (rule.from, rule.to))
            .collect::<Vec<_>>();
        let trace = apply_replacements_detailed(text, &rules);
        Ok(json!({
            "original": text,
            "rewritten": trace.rewritten,
            "changed": trace.rewritten != text,
            "applied": trace.applied
        }))
    }
}

fn optional_query(arguments: &Value) -> Result<Option<&str>, String> {
    match arguments.get("query") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let query = value
                .as_str()
                .ok_or_else(|| "query 必须是字符串。".to_string())?
                .trim();
            if query.is_empty() {
                Ok(None)
            } else {
                Ok(Some(query))
            }
        }
    }
}

fn optional_limit(arguments: &Value) -> Result<usize, String> {
    match arguments.get("limit") {
        None | Some(Value::Null) => Ok(MAX_LIMIT),
        Some(value) => {
            let limit = value
                .as_u64()
                .ok_or_else(|| "limit 必须是正整数。".to_string())?
                as usize;
            if limit == 0 {
                return Err("limit 至少为 1。".into());
            }
            Ok(limit.min(MAX_LIMIT))
        }
    }
}

fn matches_query(haystack: &str, query: Option<&str>) -> bool {
    let Some(query) = query else {
        return true;
    };
    haystack.to_lowercase().contains(&query.to_lowercase())
}

fn filter_hotwords(words: &[String], query: Option<&str>, limit: usize) -> Vec<String> {
    words
        .iter()
        .filter(|word| matches_query(word, query))
        .take(limit)
        .cloned()
        .collect()
}

fn filter_replacements(
    rules: &[ReplacementRule],
    query: Option<&str>,
    limit: usize,
) -> Vec<ReplacementRule> {
    rules
        .iter()
        .filter(|rule| matches_query(&rule.from, query) || matches_query(&rule.to, query))
        .take(limit)
        .cloned()
        .collect()
}

/// 最终写法优先：替换词的 to，再加上没有对应 from 的热词。
fn canonical_terms(hotwords: &[String], replacements: &[ReplacementRule]) -> Vec<String> {
    let from_keys = replacements
        .iter()
        .map(|rule| rule.from.to_lowercase())
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for rule in replacements {
        if seen.insert(rule.to.to_lowercase()) {
            out.push(rule.to.clone());
        }
    }
    for word in hotwords {
        if from_keys.contains(&word.to_lowercase()) {
            continue;
        }
        if seen.insert(word.to_lowercase()) {
            out.push(word.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 临时目录用进程内计数器区分，不用时间戳：同 crate 的测试是并行跑的，
    /// 纳秒精度不够，两个测试可能拿到同一个目录名，然后互相删掉对方的文件。
    fn test_dir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "jackvoice-agent-bridge-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seed_glossary(dir: &Path) {
        fs::write(
            dir.join("hotwords.json"),
            serde_json::to_vec(&["智能剪口播", "Broll", "达芬奇"]).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("replacements.json"),
            serde_json::to_vec(&[
                ReplacementRule {
                    from: "Broll".into(),
                    to: "B roll".into(),
                },
                ReplacementRule {
                    from: "绘画".into(),
                    to: "会话".into(),
                },
                ReplacementRule {
                    from: "DaVinci Playground".into(),
                    to: "DaVinci Playground".into(),
                },
            ])
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn glossary_returns_canonical_terms_without_recognition_forms() {
        let dir = test_dir();
        seed_glossary(&dir);
        let tools = AgentTools::new(dir.clone());
        let result = tools.call("get_glossary", json!({})).unwrap();
        let terms = result["canonicalTerms"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect::<Vec<_>>();
        assert!(terms.contains(&"B roll".into()));
        assert!(terms.contains(&"智能剪口播".into()));
        assert!(terms.contains(&"达芬奇".into()));
        assert!(!terms.iter().any(|term| term == "Broll"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn apply_replacements_uses_local_rules() {
        let dir = test_dir();
        seed_glossary(&dir);
        let tools = AgentTools::new(dir.clone());
        let result = tools
            .call("apply_replacements", json!({"text": "打开 Broll 继续绘画"}))
            .unwrap();
        assert_eq!(result["rewritten"], "打开 B roll 继续会话");
        assert!(result["changed"].as_bool().unwrap());
        assert_eq!(result["applied"].as_array().unwrap().len(), 2);
        fs::remove_dir_all(dir).unwrap();
    }

    fn seed_memos(dir: &Path) {
        fs::write(
            dir.join("memos.json"),
            r#"{"memos":[
              {"id":"m-1","text":"写周报","createdAtMs":3000,"completed":false,"source":"voice"},
              {"id":"m-2","text":"买 HDMI 线","createdAtMs":2000,"completed":false,"source":"clipboard"},
              {"id":"m-3","text":"已经做完的事","createdAtMs":1000,"completed":true,"source":"manual"}
            ]}"#,
        )
        .unwrap();
    }

    #[test]
    fn memos_default_to_pending_only() {
        let dir = test_dir();
        seed_memos(&dir);
        let tools = AgentTools::new(dir.clone());
        let result = tools.call("get_memos", json!({})).unwrap();
        let texts = result["memos"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|memo| memo["text"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["写周报", "买 HDMI 线"]);
        assert_eq!(result["pendingCount"], 2);
        assert_eq!(result["includeCompleted"], false);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn memos_expose_source_and_can_include_completed() {
        let dir = test_dir();
        seed_memos(&dir);
        let tools = AgentTools::new(dir.clone());
        let result = tools
            .call("get_memos", json!({"includeCompleted": true}))
            .unwrap();
        assert_eq!(result["count"], 3);
        let sources = result["memos"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|memo| memo["source"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        assert_eq!(sources, vec!["voice", "clipboard", "manual"]);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn memos_query_filters_by_text() {
        let dir = test_dir();
        seed_memos(&dir);
        let tools = AgentTools::new(dir.clone());
        let result = tools.call("get_memos", json!({"query": "hdmi"})).unwrap();
        assert_eq!(result["count"], 1);
        assert_eq!(result["memos"][0]["text"], "买 HDMI 线");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn memos_are_empty_without_a_store() {
        let dir = test_dir();
        let tools = AgentTools::new(dir.clone());
        let result = tools.call("get_memos", json!({})).unwrap();
        assert_eq!(result["count"], 0);
        assert_eq!(result["pendingCount"], 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn status_reports_memo_counts_and_read_only_capabilities() {
        let dir = test_dir();
        seed_glossary(&dir);
        seed_memos(&dir);
        let tools = AgentTools::new(dir.clone());
        let status = tools.call("get_status", json!({})).unwrap();
        assert_eq!(status["memoCount"], 2);
        assert_eq!(status["capabilities"]["readMemos"], true);
        assert_eq!(status["capabilities"]["writeMemos"], false);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn query_filters_hotwords_and_replacements() {
        let dir = test_dir();
        seed_glossary(&dir);
        let tools = AgentTools::new(dir.clone());
        let hotwords = tools
            .call("get_hotwords", json!({"query": "剪口"}))
            .unwrap();
        assert_eq!(hotwords["hotwords"], json!(["智能剪口播"]));
        let replacements = tools
            .call("get_replacements", json!({"query": "b roll"}))
            .unwrap();
        assert_eq!(replacements["count"], 1);
        fs::remove_dir_all(dir).unwrap();
    }
}

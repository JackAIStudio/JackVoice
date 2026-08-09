use crate::storage;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 火山引擎识别形：平台会吞空格/标点；长连写实测可用，放宽到 32。
pub const VOLC_MAX_HOTWORD_CHARS: usize = 32;
/// 火山平台热词表上限。
pub const MAX_HOTWORDS: usize = 5000;

const REPLACEMENTS_FILE: &str = "replacements.json";

/// 用户手动配置的替换规则：把识别结果中的 A 换成 B。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplacementRule {
    /// 匹配文本（识别结果里出现的形态）。
    pub from: String,
    /// 替换成的文本（用户想要的最终形态）。
    pub to: String,
}

fn hotwords_path(dir: &Path) -> PathBuf {
    dir.join("hotwords.json")
}

fn replacements_path(dir: &Path) -> PathBuf {
    dir.join(REPLACEMENTS_FILE)
}

pub fn load(dir: &Path) -> Vec<String> {
    fs::read_to_string(hotwords_path(dir))
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default()
}

pub fn load_replacements(dir: &Path) -> Vec<ReplacementRule> {
    fs::read_to_string(replacements_path(dir))
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<ReplacementRule>>(&raw).ok())
        .unwrap_or_default()
}

/// 火山热词：去空格/标点后的字母数字串。
pub fn is_valid_volc_hotword(text: &str) -> bool {
    let normalized = normalize_volc_hotword(text);
    if normalized.is_empty() {
        return false;
    }
    if normalized.chars().count() > VOLC_MAX_HOTWORD_CHARS {
        return false;
    }
    normalized.chars().all(|c| c.is_alphanumeric())
}

pub fn normalize_volc_hotword(text: &str) -> String {
    text.trim()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

pub fn max_hotwords() -> usize {
    MAX_HOTWORDS
}

/// 清洗热词：火山模式存识别形（去空格/标点）。
pub fn sanitize(words: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in words {
        let cleaned = normalize_volc_hotword(word);
        if cleaned.is_empty() || !is_valid_volc_hotword(&cleaned) {
            continue;
        }
        if out.iter().any(|e| e.eq_ignore_ascii_case(&cleaned)) {
            continue;
        }
        out.push(cleaned);
        if out.len() >= max_hotwords() {
            break;
        }
    }
    out
}

/// 清洗用户手写的替换规则。
pub fn sanitize_replacements(rules: &[ReplacementRule]) -> Vec<ReplacementRule> {
    let mut out: Vec<ReplacementRule> = Vec::new();
    for rule in rules {
        let from = rule.from.trim().to_string();
        let to = rule.to.trim().to_string();
        if from.is_empty() || to.is_empty() {
            continue;
        }
        // 允许 from == to 作为“短语锁”，避免短词规则误伤长短语。
        // 例如保留 “DaVinci Playground”，防止被 “DaVinci → 达芬奇” 拆坏。
        if out.iter().any(|r| r.from == from) {
            // 同 from 后写覆盖先写
            if let Some(existing) = out.iter_mut().find(|r| r.from == from) {
                existing.to = to;
            }
            continue;
        }
        out.push(ReplacementRule { from, to });
    }
    // 长 from 优先，避免短词抢先。
    out.sort_by(|a, b| b.from.chars().count().cmp(&a.from.chars().count()));
    out
}

/// 识别时下发的热词列表。
pub fn recognition_words(words: &[String]) -> Vec<String> {
    sanitize(words)
}

/// 仅使用用户配置的替换规则（不会从热词自动生成）。
pub fn user_replacement_rules(dir: &Path) -> Vec<(String, String)> {
    sanitize_replacements(&load_replacements(dir))
        .into_iter()
        .map(|r| (r.from, r.to))
        .collect()
}

pub fn format_volc_boosting_table_file(words: &[String]) -> String {
    recognition_words(words)
        .into_iter()
        .map(|w| format!("{w}|10"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 用户替换规则 → 火山替换词表文件：`from|to`。
/// 当前产品策略：替换词仅本地生效，此格式化函数保留备用。
/// 云端只应同步真正改写规则；from==to 的“短语锁”仅本地最长匹配使用。
#[allow(dead_code)]
pub fn format_volc_correct_table_file(rules: &[ReplacementRule]) -> String {
    sanitize_replacements(rules)
        .into_iter()
        .filter(|r| r.from != r.to)
        .map(|r| format!("{}|{}", r.from, r.to))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn apply_replacements(text: &str, rules: &[(String, String)]) -> String {
    if text.is_empty() || rules.is_empty() {
        return text.to_string();
    }
    // 单次从左到右最长匹配：在原文位置选最长 from，避免
    // 1) 短词先吃掉长词的一部分；
    // 2) 先替换长词后再被短词二次误伤（如 DaVinciPlayground → DaVinci Playground → 达芬奇 Playground）。
    let patterns: Vec<(Vec<char>, &str)> = rules
        .iter()
        .map(|(from, to)| (from.to_lowercase().chars().collect::<Vec<_>>(), to.as_str()))
        .filter(|(from, _)| !from.is_empty())
        .collect();
    if patterns.is_empty() {
        return text.to_string();
    }

    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    let mut out = String::with_capacity(text.len());
    while i < chars.len() {
        let mut best_len = 0usize;
        let mut best_to: Option<&str> = None;
        for (pattern, replacement) in &patterns {
            let plen = pattern.len();
            if plen <= best_len || i + plen > chars.len() {
                continue;
            }
            let matched = chars[i..i + plen]
                .iter()
                .map(|c| c.to_lowercase().next().unwrap_or(*c))
                .eq(pattern.iter().copied());
            if matched {
                best_len = plen;
                best_to = Some(replacement);
            }
        }
        if let Some(replacement) = best_to {
            out.push_str(replacement);
            i += best_len;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

pub fn save(dir: &Path, words: &[String]) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(words).map_err(|e| e.to_string())?;
    storage::write_atomic(&hotwords_path(dir), &raw, true)
}

pub fn save_replacements(dir: &Path, rules: &[ReplacementRule]) -> Result<(), String> {
    let cleaned = sanitize_replacements(rules);
    let raw = serde_json::to_vec_pretty(&cleaned).map_err(|e| e.to_string())?;
    storage::write_atomic(&replacements_path(dir), &raw, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_volc_stores_recognition_form_only() {
        let words = vec![
            "B roll".to_string(),
            "fun-asr".to_string(),
            "OpenClaw".to_string(),
        ];
        let cleaned = sanitize(&words);
        assert_eq!(cleaned, vec!["Broll", "funasr", "OpenClaw"]);
    }

    #[test]
    fn sanitize_replacements_allows_identity_phrase_locks() {
        let rules = vec![
            ReplacementRule {
                from: "Broll".into(),
                to: "B roll".into(),
            },
            ReplacementRule {
                from: "  ".into(),
                to: "x".into(),
            },
            ReplacementRule {
                from: "DaVinci Playground".into(),
                to: "DaVinci Playground".into(),
            },
        ];
        let cleaned = sanitize_replacements(&rules);
        assert_eq!(cleaned.len(), 2);
        assert_eq!(cleaned[0].from, "DaVinci Playground");
        assert_eq!(cleaned[0].to, "DaVinci Playground");
        assert_eq!(cleaned[1].from, "Broll");
        assert_eq!(cleaned[1].to, "B roll");
    }

    #[test]
    fn apply_replacements_uses_user_rules() {
        let rules = vec![
            ("ChromeDevTools".into(), "Chrome DevTools".into()),
            ("Broll".into(), "B roll".into()),
        ];
        assert_eq!(
            apply_replacements("导出 Broll 到 ChromeDevTools", &rules),
            "导出 B roll 到 Chrome DevTools"
        );
    }

    #[test]
    fn apply_replacements_prefers_longest_match_and_avoids_rescan() {
        let rules = sanitize_replacements(&[
            ReplacementRule {
                from: "DaVinci".into(),
                to: "达芬奇".into(),
            },
            ReplacementRule {
                from: "DaVinciPlayground".into(),
                to: "DaVinci Playground".into(),
            },
            ReplacementRule {
                from: "DaVinci Playground".into(),
                to: "DaVinci Playground".into(),
            },
        ])
        .into_iter()
        .map(|r| (r.from, r.to))
        .collect::<Vec<_>>();

        assert_eq!(
            apply_replacements("打开 DaVinciPlayground 和 DaVinci", &rules),
            "打开 DaVinci Playground 和 达芬奇"
        );
        assert_eq!(
            apply_replacements("继续用 DaVinci Playground", &rules),
            "继续用 DaVinci Playground"
        );
    }

    #[test]
    fn max_hotwords_is_volc_platform_limit() {
        assert_eq!(max_hotwords(), 5000);
    }
}

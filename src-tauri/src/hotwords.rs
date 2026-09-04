pub use jackvoice_agent_bridge::hotwords::*;

use crate::storage;
use std::path::Path;

pub fn save(dir: &Path, words: &[String]) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(words).map_err(|e| e.to_string())?;
    storage::write_atomic(&hotwords_path(dir), &raw, true)
}

pub fn save_replacements(dir: &Path, rules: &[ReplacementRule]) -> Result<(), String> {
    let cleaned = sanitize_replacements(rules);
    let raw = serde_json::to_vec_pretty(&cleaned).map_err(|e| e.to_string())?;
    storage::write_atomic(&replacements_path(dir), &raw, true)
}

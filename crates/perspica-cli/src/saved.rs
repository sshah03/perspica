//! Analyses saved to disk, keyed by the exact diff they describe, so reloading
//! the viewer or running perspica again doesn't mean paying for the same
//! analysis twice. Stored in the repository's git directory (`.git/perspica/`),
//! or `~/.cache/perspica/` outside a repository.

use crate::{intel, MultiFileResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Saved analyses kept per directory; older ones are removed.
const MAX_SAVED: usize = 50;

#[derive(Debug, Serialize, Deserialize)]
pub struct SavedAnalysis {
    pub provider: String,
    pub model: String,
    /// "standard" | "thorough"
    pub depth: String,
    /// Unix seconds.
    pub saved_at: u64,
    pub groups: Vec<intel::IntentGroup>,
    pub summary: String,
    #[serde(default)]
    pub concerns: Vec<String>,
}

/// Identifies a diff: the perspica version (entry ids depend on it) and every
/// file's paths and contents. Stable across runs (FNV-1a, not std's hasher).
pub fn key(multi: &MultiFileResult) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes.iter().chain(&[0xff]) {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(env!("CARGO_PKG_VERSION").as_bytes());
    for f in &multi.files {
        feed(f.old_path.as_bytes());
        feed(f.new_path.as_bytes());
        feed(f.old_source.as_bytes());
        feed(f.new_source.as_bytes());
    }
    format!("{h:016x}")
}

fn dir() -> Option<PathBuf> {
    let git = std::process::Command::new("git").args(["rev-parse", "--path-format=absolute", "--git-common-dir"]).output().ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    match git {
        Some(g) => Some(PathBuf::from(g).join("perspica")),
        None => {
            let base = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
            Some(base.join("perspica"))
        }
    }
}

fn path(key: &str) -> Option<PathBuf> {
    Some(dir()?.join(format!("analysis-{key}.json")))
}

pub fn load(key: &str) -> Option<SavedAnalysis> {
    let text = std::fs::read_to_string(path(key)?).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save(key: &str, analysis: &SavedAnalysis) {
    let Some(p) = path(key) else { return };
    let write = || -> std::io::Result<()> {
        let dir = p.parent().expect("analysis path has a parent");
        std::fs::create_dir_all(dir)?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(analysis).unwrap_or_default())?;
        std::fs::rename(&tmp, &p)?;
        prune(dir);
        Ok(())
    };
    if let Err(e) = write() {
        eprintln!("perspica: couldn't save the analysis ({e}); it will be lost on restart");
    }
}

/// Keep the newest `MAX_SAVED` analyses.
fn prune(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("analysis-") && e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if files.len() <= MAX_SAVED {
        return;
    }
    files.sort();
    for (_, p) in &files[..files.len() - MAX_SAVED] {
        let _ = std::fs::remove_file(p);
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Put a saved analysis into the result.
pub fn apply(multi: &mut MultiFileResult, s: SavedAnalysis) {
    multi.intent_groups = Some(s.groups);
    multi.summary = Some(s.summary).filter(|x| !x.trim().is_empty());
    multi.concerns = s.concerns;
    multi.llm_provider = Some(s.provider);
    multi.llm_model = Some(s.model);
    multi.llm_saved_at = Some(s.saved_at);
}

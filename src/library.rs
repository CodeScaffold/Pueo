//! Query history (~/.local/share/pueo/history.jsonl) and saved queries
//! (~/.config/pueo/saved_queries.json).

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Keep the history file from growing without bound.
const MAX_HISTORY: usize = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub connection_id: String,
    pub sql: String,
    /// Unix timestamp (seconds).
    pub at: i64,
    pub ok: bool,
    pub millis: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedQuery {
    pub id: String,
    pub connection_id: String,
    pub name: String,
    pub sql: String,
}

fn history_path() -> PathBuf {
    crate::config::data_dir().join("history.jsonl")
}

fn saved_path() -> PathBuf {
    crate::config::config_dir().join("saved_queries.json")
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn read_all_history() -> Vec<HistoryEntry> {
    let Ok(file) = std::fs::File::open(history_path()) else { return Vec::new() };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

pub fn append_history(entry: &HistoryEntry) -> Result<()> {
    let path = history_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{}", serde_json::to_string(entry)?)?;
    drop(file);

    // Occasionally compact to the newest entries.
    if std::fs::metadata(&path)?.len() > 4 * 1024 * 1024 {
        let all = read_all_history();
        let keep = &all[all.len().saturating_sub(MAX_HISTORY)..];
        let mut out = String::new();
        for e in keep {
            out.push_str(&serde_json::to_string(e)?);
            out.push('\n');
        }
        std::fs::write(&path, out)?;
    }
    Ok(())
}

/// Newest first.
pub fn load_history(connection_id: &str, limit: usize) -> Vec<HistoryEntry> {
    let mut entries: Vec<HistoryEntry> =
        read_all_history().into_iter().filter(|e| e.connection_id == connection_id).collect();
    entries.reverse();
    entries.truncate(limit);
    entries
}

pub fn clear_history(connection_id: &str) -> Result<()> {
    let keep: Vec<HistoryEntry> = read_all_history().into_iter().filter(|e| e.connection_id != connection_id).collect();
    let mut out = String::new();
    for e in &keep {
        out.push_str(&serde_json::to_string(e)?);
        out.push('\n');
    }
    std::fs::write(history_path(), out)?;
    Ok(())
}

fn read_saved() -> Vec<SavedQuery> {
    std::fs::read(saved_path())
        .ok()
        .and_then(|d| serde_json::from_slice(&d).ok())
        .unwrap_or_default()
}

fn write_saved(all: &[SavedQuery]) -> Result<()> {
    let path = saved_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(all)?)?;
    Ok(())
}

pub fn load_saved(connection_id: &str) -> Vec<SavedQuery> {
    let mut v: Vec<SavedQuery> = read_saved().into_iter().filter(|q| q.connection_id == connection_id).collect();
    v.sort_by_key(|q| q.name.to_lowercase());
    v
}

/// Insert or replace (by name within the connection).
pub fn save_query(connection_id: &str, name: &str, sql: &str) -> Result<()> {
    let mut all = read_saved();
    match all.iter_mut().find(|q| q.connection_id == connection_id && q.name == name) {
        Some(q) => q.sql = sql.to_string(),
        None => all.push(SavedQuery {
            id: uuid::Uuid::new_v4().to_string(),
            connection_id: connection_id.to_string(),
            name: name.to_string(),
            sql: sql.to_string(),
        }),
    }
    write_saved(&all)
}

pub fn delete_saved(id: &str) -> Result<()> {
    let mut all = read_saved();
    all.retain(|q| q.id != id);
    write_saved(&all)
}

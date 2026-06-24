//! Claude Code data sources.
//!
//! Tokens come from session transcripts under `~/.claude/projects/**/*.jsonl`
//! (one JSON object per line; assistant turns carry `message.usage`).
//! Utilization comes from `~/.drainage/claude_ratelimit.jsonl`, written live by
//! the statusline collector (it cannot be reconstructed from transcripts).

use crate::model::{Harness, Provider, TokenEvent, UtilSnapshot};
use anyhow::Result;
use serde_json::Value;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use walkdir::WalkDir;

pub fn projects_dir() -> PathBuf {
    home().join(".claude").join("projects")
}

pub fn ratelimit_log() -> PathBuf {
    home().join(".drainage").join("claude_ratelimit.jsonl")
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

/// Parse an ISO-8601 / RFC-3339 timestamp into unix seconds.
fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Walk every transcript and collect typed token events, deduped by message id.
pub fn token_events() -> Result<Vec<TokenEvent>> {
    let dir = projects_dir();
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if !dir.exists() {
        return Ok(out);
    }

    for entry in WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
    {
        let file = match File::open(entry.path()) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if line.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
                continue;
            }
            let msg = match v.get("message") {
                Some(m) => m,
                None => continue,
            };
            let usage = match msg.get("usage") {
                Some(u) => u,
                None => continue,
            };
            let id = msg
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            // Dedup the same API response logged in multiple transcripts.
            let dedup_key = if id.is_empty() {
                format!("{}:{}", entry.path().display(), v.get("uuid").and_then(|u| u.as_str()).unwrap_or(""))
            } else {
                id.clone()
            };
            if !seen.insert(dedup_key.clone()) {
                continue;
            }
            let ts = v
                .get("timestamp")
                .and_then(|t| t.as_str())
                .and_then(parse_ts)
                .unwrap_or(0);
            let model = msg
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
                .to_string();
            // Skip injected/synthetic assistant turns — they aren't billed.
            if model.starts_with('<') {
                continue;
            }
            let g = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
            out.push(TokenEvent {
                ts,
                harness: Harness::ClaudeCode,
                provider: Provider::Anthropic,
                model,
                input: g("input_tokens"),
                output: g("output_tokens"),
                cache_read: g("cache_read_input_tokens"),
                cache_write: g("cache_creation_input_tokens"),
                reasoning: 0,
                id: dedup_key,
            });
        }
    }
    out.sort_by_key(|e| e.ts);
    Ok(out)
}

/// Read the live utilization snapshots collected by the statusline hook.
pub fn util_snapshots() -> Result<Vec<UtilSnapshot>> {
    let path = ratelimit_log();
    let mut out = Vec::new();
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(_) => return Ok(out),
    };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let num = |k: &str| v.get(k).and_then(|x| x.as_f64());
        let int = |k: &str| v.get(k).and_then(|x| x.as_i64());
        out.push(UtilSnapshot {
            ts: int("ts").unwrap_or(0),
            provider: Provider::Anthropic,
            source: Harness::ClaudeCode,
            five_pct: num("five_pct"),
            week_pct: num("week_pct"),
            five_reset: int("five_reset"),
            week_reset: int("week_reset"),
        });
    }
    out.sort_by_key(|s| s.ts);
    Ok(out)
}

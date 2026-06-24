//! Codex (ChatGPT subscription) data sources.
//!
//! Both tokens and utilization live in `~/.codex/sessions/**/rollout-*.jsonl`.
//! Token usage is reported CUMULATIVELY per session (`total_token_usage`), so we
//! diff consecutive readings within a file to recover per-turn spend. Rate-limit
//! state is mirrored into the same stream (`codex.rate_limits` /
//! `x-codex-*-used-percent`), so Codex utilization is partially backfillable.
//!
//! Codex's schema has drifted across versions, so we scan each line recursively
//! for the shapes we care about rather than assuming a fixed envelope.

use crate::model::{Harness, Provider, TokenEvent, UtilSnapshot};
use anyhow::Result;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use walkdir::WalkDir;

pub fn sessions_dir() -> PathBuf {
    let base = std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into())).join(".codex")
        });
    base.join("sessions")
}

fn parse_ts(v: &Value) -> Option<i64> {
    // Accept either an RFC-3339 string or an epoch number under common keys.
    for k in ["timestamp", "ts", "time", "created_at"] {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(dt.timestamp());
            }
        }
        if let Some(n) = v.get(k).and_then(|x| x.as_i64()) {
            // Heuristic: treat large numbers as ms.
            return Some(if n > 100_000_000_000 { n / 1000 } else { n });
        }
    }
    None
}

/// Depth-first search for the first object containing every key in `keys`.
fn find_with<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    match v {
        Value::Object(map) => {
            if keys.iter().all(|k| map.contains_key(*k)) {
                return Some(v);
            }
            for (_, val) in map {
                if let Some(found) = find_with(val, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|x| find_with(x, keys)),
        _ => None,
    }
}

pub fn token_events() -> Result<Vec<TokenEvent>> {
    let dir = sessions_dir();
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }

    for entry in WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().extension().map(|x| x == "jsonl").unwrap_or(false)
                && e.file_name().to_string_lossy().starts_with("rollout-")
        })
    {
        let file = match File::open(entry.path()) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let session = entry.file_name().to_string_lossy().to_string();
        // Track cumulative totals to recover per-turn deltas within this file.
        let (mut prev_in, mut prev_out, mut prev_cached, mut prev_reason) = (0u64, 0u64, 0u64, 0u64);
        let mut idx = 0u64;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if line.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(tt) = find_with(&v, &["total_token_usage"]) else {
                continue;
            };
            let usage = &tt["total_token_usage"];
            let g = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
            let (cin, cout, ccached, creason) = (
                g("input_tokens"),
                g("output_tokens"),
                g("cached_input_tokens"),
                g("reasoning_output_tokens"),
            );
            // Cumulative -> per-turn delta (saturating; resets handled gracefully).
            let d_in = cin.saturating_sub(prev_in);
            let d_out = cout.saturating_sub(prev_out);
            let d_cached = ccached.saturating_sub(prev_cached);
            let d_reason = creason.saturating_sub(prev_reason);
            prev_in = cin.max(prev_in);
            prev_out = cout.max(prev_out);
            prev_cached = ccached.max(prev_cached);
            prev_reason = creason.max(prev_reason);

            if d_in + d_out + d_cached + d_reason == 0 {
                continue;
            }
            let ts = parse_ts(&v).unwrap_or(0);
            let model = find_with(&v, &["model"])
                .and_then(|m| m.get("model"))
                .and_then(|m| m.as_str())
                .unwrap_or("gpt-codex")
                .to_string();
            idx += 1;
            out.push(TokenEvent {
                ts,
                harness: Harness::Codex,
                provider: Provider::OpenAI,
                model,
                input: d_in.saturating_sub(d_cached), // non-cached input
                output: d_out,
                cache_read: d_cached,
                cache_write: 0,
                reasoning: d_reason,
                id: format!("{session}:{idx}"),
            });
        }
    }
    out.sort_by_key(|e| e.ts);
    Ok(out)
}

pub fn util_snapshots() -> Result<Vec<UtilSnapshot>> {
    let dir = sessions_dir();
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for entry in WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().extension().map(|x| x == "jsonl").unwrap_or(false)
                && e.file_name().to_string_lossy().starts_with("rollout-")
        })
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
            let Some(rl) = find_with(&v, &["primary", "secondary"]) else {
                continue;
            };
            let pct = |b: &str| {
                rl.get(b)
                    .and_then(|x| x.get("used_percent"))
                    .and_then(|x| x.as_f64())
            };
            let reset = |b: &str| {
                rl.get(b)
                    .and_then(|x| x.get("reset_at"))
                    .and_then(|x| x.as_i64())
            };
            if pct("primary").is_none() && pct("secondary").is_none() {
                continue;
            }
            out.push(UtilSnapshot {
                ts: parse_ts(&v).unwrap_or(0),
                provider: Provider::OpenAI,
                source: Harness::Codex,
                five_pct: pct("primary"),
                week_pct: pct("secondary"),
                five_reset: reset("primary"),
                week_reset: reset("secondary"),
            });
        }
    }
    out.sort_by_key(|s| s.ts);
    Ok(out)
}

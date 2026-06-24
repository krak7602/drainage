//! oh-my-pi (`omp`) data sources.
//!
//! omp is the richest source: it persists BOTH token spend and a subscription
//! utilization time-series locally, and supports Claude Pro/Max + ChatGPT/Codex
//! subscription OAuth — so its windowed usage is exactly what drainage measures.
//!
//! - Tokens: `~/.omp/agent/sessions/**/*.jsonl`, assistant messages carry
//!   `message.usage.{input,output,cacheRead,cacheWrite,reasoningTokens}` plus
//!   `model`, `provider`, and an epoch-ms `timestamp`.
//! - Utilization: SQLite `~/.omp/agent/agent.db`, table `usage_history`
//!   (`recorded_at, provider, account_key, limit_id, used_fraction, resets_at`),
//!   ~1 row/hour — durable and BACKFILLABLE, no proxy needed.
//!
//! Honors `PI_CODING_AGENT_DIR` / `PI_CONFIG_DIR`; falls back to the Linux XDG
//! layout if the default `~/.omp` tree is absent.

use crate::model::{Harness, Provider, TokenEvent, UtilSnapshot};
use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use walkdir::WalkDir;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

/// The `~/.omp/agent` directory (or its overrides).
fn agent_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PI_CODING_AGENT_DIR") {
        return PathBuf::from(d);
    }
    let config_root = std::env::var("PI_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".omp"));
    let agent = config_root.join("agent");
    if agent.exists() {
        return agent;
    }
    // Linux XDG layout (flattens the `agent/` segment) as a fallback.
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        let p = PathBuf::from(xdg).join("omp");
        if p.exists() {
            return p;
        }
    }
    agent
}

fn sessions_dir() -> PathBuf {
    agent_dir().join("sessions")
}

fn db_path() -> PathBuf {
    agent_dir().join("agent.db")
}

/// Normalize an epoch that might be in seconds or milliseconds to seconds.
fn to_secs(n: i64) -> i64 {
    if n > 100_000_000_000 {
        n / 1000
    } else {
        n
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
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
    {
        let file = match File::open(entry.path()) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let session = entry.file_name().to_string_lossy().to_string();
        let mut idx = 0u64;
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if line.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("message") {
                continue;
            }
            let msg = match v.get("message") {
                Some(m) => m,
                None => continue,
            };
            if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                continue;
            }
            let usage = match msg.get("usage") {
                Some(u) => u,
                None => continue,
            };
            let g = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
            let (input, output, cache_read, cache_write, reasoning) = (
                g("input"),
                g("output"),
                g("cacheRead"),
                g("cacheWrite"),
                g("reasoningTokens"),
            );
            if input + output + cache_read + cache_write == 0 {
                continue;
            }
            let model = msg
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
                .to_string();
            let provider_str = msg
                .get("upstreamProvider")
                .and_then(|p| p.as_str())
                .or_else(|| msg.get("provider").and_then(|p| p.as_str()))
                .unwrap_or("");
            let ts = msg
                .get("timestamp")
                .and_then(|t| t.as_i64())
                .map(to_secs)
                .unwrap_or(0);
            idx += 1;
            out.push(TokenEvent {
                ts,
                harness: Harness::Omp,
                provider: Provider::classify(provider_str),
                model,
                input,
                output,
                cache_read,
                cache_write,
                reasoning,
                id: msg
                    .get("id")
                    .and_then(|i| i.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| format!("{session}:{idx}")),
            });
        }
    }
    out.sort_by_key(|e| e.ts);
    Ok(out)
}

pub fn util_snapshots() -> Result<Vec<UtilSnapshot>> {
    let path = db_path();
    let mut out = Vec::new();
    if !path.exists() {
        return Ok(out);
    }
    let conn = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(_) => return Ok(out),
    };
    let mut stmt = match conn.prepare(
        "SELECT recorded_at, provider, account_key, limit_id, used_fraction, resets_at \
         FROM usage_history",
    ) {
        Ok(s) => s,
        Err(_) => return Ok(out), // table may not exist on older omp
    };
    let rows = match stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<f64>>(4)?,
            r.get::<_, Option<i64>>(5)?,
        ))
    }) {
        Ok(r) => r,
        Err(_) => return Ok(out),
    };

    // Group rows of the same (time, provider, account) into one snapshot,
    // folding the per-window limit rows (5h / 7d) together.
    type Key = (i64, String, String);
    let mut groups: BTreeMap<Key, UtilSnapshot> = BTreeMap::new();
    for row in rows.flatten() {
        let (recorded_at, provider, account, limit_id, used_fraction, resets_at) = row;
        let ts = to_secs(recorded_at);
        let key = (ts, provider.clone(), account.unwrap_or_default());
        let snap = groups.entry(key).or_insert_with(|| UtilSnapshot {
            ts,
            provider: Provider::classify(&provider),
            source: Harness::Omp,
            five_pct: None,
            week_pct: None,
            five_reset: None,
            week_reset: None,
        });
        let pct = used_fraction.map(|f| f * 100.0);
        let reset = resets_at.map(to_secs);
        let lid = limit_id.to_ascii_lowercase();
        if lid.contains("5h") || lid.contains("five") || lid.contains("hour") {
            snap.five_pct = pct;
            snap.five_reset = reset;
        } else if lid.contains("7d") || lid.contains("seven") || lid.contains("week") {
            snap.week_pct = pct;
            snap.week_reset = reset;
        }
    }

    out.extend(groups.into_values());
    out.sort_by_key(|s| s.ts);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Provider;

    /// Build a synthetic agent.db with the documented schema and verify the
    /// reader folds the per-window rows into snapshots (omp isn't installed, so
    /// this is how we validate the SQLite path without real data).
    #[test]
    fn reads_usage_history() {
        let dir = std::env::temp_dir().join("drainage_omp_test");
        let _ = std::fs::create_dir_all(dir.join("agent"));
        let db = dir.join("agent").join("agent.db");
        let _ = std::fs::remove_file(&db);
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE usage_history (recorded_at INTEGER, provider TEXT, \
                 account_key TEXT, limit_id TEXT, label TEXT, window_label TEXT, \
                 used_fraction REAL, status TEXT, resets_at INTEGER);
                 INSERT INTO usage_history VALUES (1750000000000,'anthropic','acct','5h','','',0.18,'ok',1750018000000);
                 INSERT INTO usage_history VALUES (1750000000000,'anthropic','acct','7d','','',0.74,'warning',1750600000000);
                 INSERT INTO usage_history VALUES (1750003600000,'openai-codex','acct2','5h','','',0.40,'ok',1750021600000);",
            )
            .unwrap();
        }
        // Point the reader at our temp tree.
        std::env::set_var("PI_CODING_AGENT_DIR", dir.join("agent"));
        let snaps = util_snapshots().unwrap();
        std::env::remove_var("PI_CODING_AGENT_DIR");

        assert_eq!(snaps.len(), 2);
        let anthropic = snaps.iter().find(|s| s.provider == Provider::Anthropic).unwrap();
        assert_eq!(anthropic.five_pct, Some(18.0));
        assert_eq!(anthropic.week_pct, Some(74.0));
        assert_eq!(anthropic.ts, 1750000000); // ms normalized to seconds
        let openai = snaps.iter().find(|s| s.provider == Provider::OpenAI).unwrap();
        assert_eq!(openai.five_pct, Some(40.0));
    }
}

//! Self-installing collector for Claude Code.
//!
//! Claude Code allows exactly one statusline command, so drainage installs
//! itself as a *wrapper*: `drainage statusline` reads the JSON CC pipes in,
//! appends a deduped rate-limit/token snapshot (the one signal that can't be
//! backfilled), then forwards stdin to the user's original statusline and
//! passes its output straight through. `init` wires this up; `uninstall`
//! restores the original.

use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}
fn drainage_dir() -> PathBuf {
    home().join(".drainage")
}
fn config_path() -> PathBuf {
    drainage_dir().join("config.json")
}
fn log_path() -> PathBuf {
    drainage_dir().join("claude_ratelimit.jsonl")
}
fn last_path() -> PathBuf {
    drainage_dir().join(".last_rl")
}
fn settings_path() -> PathBuf {
    home().join(".claude").join("settings.json")
}

/// The statusline command drainage wrapped, if any (so we can forward + restore).
fn wrapped_command() -> Option<String> {
    let cfg = std::fs::read_to_string(config_path()).ok()?;
    let v: Value = serde_json::from_str(&cfg).ok()?;
    v.get("wrapped_statusline")
        .and_then(|x| x.as_str())
        .map(String::from)
}

// ---------------------------------------------------------------------------
// `drainage statusline` — the per-render hook.
// ---------------------------------------------------------------------------

pub fn statusline() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok();

    // Best-effort logging: never let a failure here break the statusline.
    let _ = log_snapshot(&input);

    // Forward to the user's original statusline, or render a minimal default.
    match wrapped_command() {
        Some(cmd) if !cmd.is_empty() => {
            if let Ok(mut child) = Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
            {
                if let Some(mut sin) = child.stdin.take() {
                    let _ = sin.write_all(input.as_bytes());
                }
                if let Ok(out) = child.wait_with_output() {
                    let _ = std::io::stdout().write_all(&out.stdout);
                }
            }
        }
        _ => print!("{}", default_line(&input)),
    }
    Ok(())
}

fn default_line(input: &str) -> String {
    let v: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    let model = v
        .get("model")
        .and_then(|m| m.get("display_name"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let five = v
        .pointer("/rate_limits/five_hour/used_percentage")
        .and_then(|x| x.as_f64());
    let week = v
        .pointer("/rate_limits/seven_day/used_percentage")
        .and_then(|x| x.as_f64());
    let mut s = format!("drainage · {model}");
    if let Some(f) = five {
        s.push_str(&format!(" · 5h:{f:.0}%"));
    }
    if let Some(w) = week {
        s.push_str(&format!(" 7d:{w:.0}%"));
    }
    s
}

/// Append a snapshot iff subscription rate-limit data is present and changed.
fn log_snapshot(input: &str) -> Result<()> {
    let v: Value = serde_json::from_str(input)?;
    let rl = match v.get("rate_limits") {
        Some(r) if !r.is_null() => r,
        _ => return Ok(()),
    };
    let five = rl.pointer("/five_hour/used_percentage").and_then(|x| x.as_f64());
    let week = rl.pointer("/seven_day/used_percentage").and_then(|x| x.as_f64());
    if five.is_none() && week.is_none() {
        return Ok(());
    }

    // Dedup: skip if neither window % changed since the last snapshot.
    let sig = format!("{:?}|{:?}", five, week);
    if let Ok(prev) = std::fs::read_to_string(last_path()) {
        if prev == sig {
            return Ok(());
        }
    }

    let ts = chrono::Utc::now().timestamp();
    let snap = serde_json::json!({
        "ts": ts,
        "session_id": v.get("session_id"),
        "model": v.pointer("/model/id"),
        "model_name": v.pointer("/model/display_name"),
        "five_pct": five,
        "five_reset": rl.pointer("/five_hour/resets_at"),
        "week_pct": week,
        "week_reset": rl.pointer("/seven_day/resets_at"),
        "ctx_in": v.pointer("/context_window/total_input_tokens"),
        "ctx_out": v.pointer("/context_window/total_output_tokens"),
        "ctx_pct": v.pointer("/context_window/used_percentage"),
        "cost_usd": v.pointer("/cost/total_cost_usd"),
        "cwd": v.pointer("/workspace/current_dir").or_else(|| v.get("cwd")),
        "version": v.get("version"),
    });

    std::fs::create_dir_all(drainage_dir())?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())?;
    writeln!(f, "{}", serde_json::to_string(&snap)?)?;
    let _ = std::fs::write(last_path(), sig);
    Ok(())
}

// ---------------------------------------------------------------------------
// `drainage init` / `drainage uninstall`
// ---------------------------------------------------------------------------

pub fn init() -> Result<()> {
    std::fs::create_dir_all(drainage_dir())?;
    let bin = std::env::current_exe().context("locating drainage binary")?;
    let our_cmd = format!("{} statusline", bin.display());

    let sp = settings_path();
    let mut settings: Value = match std::fs::read_to_string(&sp) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };

    let current = settings
        .pointer("/statusLine/command")
        .and_then(|x| x.as_str())
        .map(String::from);

    if let Some(ref c) = current {
        if c.contains("drainage") && c.contains("statusline") {
            println!("drainage is already installed as your statusline. Nothing to do.");
            return Ok(());
        }
    }

    // Back up settings.json and record what we wrapped.
    if sp.exists() {
        let _ = std::fs::copy(&sp, drainage_dir().join("settings.json.bak"));
    }
    let cfg = serde_json::json!({ "wrapped_statusline": current });
    std::fs::write(config_path(), serde_json::to_string_pretty(&cfg)?)?;

    if !settings.is_object() {
        settings = serde_json::json!({});
    }
    settings["statusLine"] = serde_json::json!({ "type": "command", "command": our_cmd });
    std::fs::create_dir_all(sp.parent().unwrap())?;
    std::fs::write(&sp, serde_json::to_string_pretty(&settings)?)?;

    println!("✓ drainage collector installed.");
    match current {
        Some(c) => println!("  Your previous statusline is preserved and still renders:\n    {c}"),
        None => println!("  No previous statusline found — drainage renders a minimal default line."),
    }
    println!("  Backup: ~/.drainage/settings.json.bak");
    println!("  Snapshots will accumulate at ~/.drainage/claude_ratelimit.jsonl as you use Claude Code.");
    println!("  Run `drainage` for the TUI, or `drainage uninstall` to restore.");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let sp = settings_path();
    let mut settings: Value = match std::fs::read_to_string(&sp) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };

    match wrapped_command() {
        Some(cmd) if !cmd.is_empty() => {
            settings["statusLine"] = serde_json::json!({ "type": "command", "command": cmd });
            println!("✓ Restored your original statusline.");
        }
        _ => {
            if let Some(obj) = settings.as_object_mut() {
                obj.remove("statusLine");
            }
            println!("✓ Removed the drainage statusline (you had none before).");
        }
    }
    std::fs::write(&sp, serde_json::to_string_pretty(&settings)?)?;
    let _ = std::fs::remove_file(config_path());
    println!("  Collected data left intact at ~/.drainage/.");
    Ok(())
}

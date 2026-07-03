mod analysis;
mod collect;
mod data;
mod levels;
mod model;
mod sources;
mod tui;

use analysis::{day_key, Window};
use anyhow::Result;
use clap::{Parser, Subcommand};
use data::{Dataset, Method};
use model::{Harness, Weights};


#[derive(Parser)]
#[command(name = "drainage", about = "Track the drifting token→usage-% exchange rate of AI subscriptions")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Launch the interactive TUI (default).
    Tui,
    /// Read all local data and print an exchange-rate report.
    Scan,
    /// Install the drainage collector into your Claude Code statusline.
    Init,
    /// Remove the drainage collector and restore your original statusline.
    Uninstall,
    /// Statusline hook: log a snapshot and forward to the wrapped statusline.
    #[command(hide = true)]
    Statusline,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Tui) {
        Cmd::Tui => tui::run(),
        Cmd::Scan => scan(),
        Cmd::Init => collect::init(),
        Cmd::Uninstall => collect::uninstall(),
        Cmd::Statusline => collect::statusline(),
    }
}

fn human(n: f64) -> String {
    if n >= 1e9 {
        format!("{:.2}B", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.2}M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.1}K", n / 1e3)
    } else {
        format!("{n:.0}")
    }
}

fn scan() -> Result<()> {
    let d = Dataset::load(Weights::default())?;

    println!("\x1b[1mdrainage scan\x1b[0m");
    println!("─────────────────────────────────────────────");
    println!("sources");
    let span = |h: Harness| -> String {
        let ts: Vec<i64> = d
            .events
            .iter()
            .filter(|e| e.harness == h && e.ts > 0)
            .map(|e| e.ts)
            .collect();
        match (ts.iter().min(), ts.iter().max()) {
            (Some(a), Some(b)) => format!("{} → {}", day_key(*a), day_key(*b)),
            _ => "—".into(),
        }
    };
    println!(
        "  claude-code : {} token events ({}), {} util snapshots",
        d.n_claude,
        span(Harness::ClaudeCode),
        d.n_claude_snaps
    );
    println!(
        "  codex       : {} token events ({}), {} util snapshots",
        d.n_codex,
        span(Harness::Codex),
        d.n_codex_snaps
    );
    println!(
        "  omp         : {} token events ({}), {} util snapshots",
        d.n_omp,
        span(Harness::Omp),
        d.n_omp_snaps
    );
    println!();

    println!("token spend by model (weighted = input·1 + output·5 + cache_write·1.25 + cache_read·0)");
    println!(
        "  {:<12} {:<24} {:>10} {:>10} {:>10} {:>8}",
        "harness", "model", "raw", "output", "weighted", "calls"
    );
    for r in d.by_model() {
        println!(
            "  {:<12} {:<24} {:>10} {:>10} {:>10} {:>8}",
            r.harness.to_string(),
            truncate(&r.model, 24),
            human(r.raw as f64),
            human(r.output as f64),
            human(r.weighted),
            r.calls,
        );
    }
    println!();

    println!("exchange rate PER MODEL  (Δ window-% per 1M weighted tokens; cache-reads excluded)");
    println!("  single = intervals a model dominated · levels = per-epoch levels-NNLS + Kalman");
    let mut any = false;
    for provider in d.providers() {
        for window in [Window::FiveHour, Window::SevenDay] {
            let mut header_done = false;
            for model in d.models(&provider) {
                let single = d.model_rate(&provider, &model, window, Method::Single);
                let levels = d.model_rate(&provider, &model, window, Method::Levels);
                if single.is_none() && levels.is_none() {
                    continue;
                }
                any = true;
                if !header_done {
                    println!("  {provider} [{}]:", window.label());
                    header_done = true;
                }
                let s = single
                    .map(|(r, n)| format!("single {r:>6.2} (n={n})"))
                    .unwrap_or_else(|| "single    —       ".into());
                let lv = levels
                    .map(|(r, k)| format!("levels {r:>6.2} ({k} epochs)"))
                    .unwrap_or_else(|| "levels   —".into());
                let drift = d
                    .model_drift_summary(&provider, &model, window, Method::Levels)
                    .map(|(r, o, c)| format!("   drift {o:.2}→{r:.2} ({c:+.0}%)"))
                    .unwrap_or_default();
                println!("      {:<22} {s}  |  {lv} %/Mtok{drift}", truncate(&model, 22));
            }
        }
    }
    if !any {
        println!("  no per-model rates yet — collector just started.");
        println!("  Claude utilization can't be backfilled; keep using your agents and it fills in.");
    }
    if d.analysis.decayed_skipped > 0 {
        println!(
            "  ({} intervals skipped: window decaying, not fillable to a rate)",
            d.analysis.decayed_skipped
        );
    }
    println!();
    println!("note: utilization is account-global; spend from claude.ai chat or other");
    println!("      sessions on the same account is invisible here and adds noise.");
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n - 1).collect();
        format!("{t}…")
    }
}

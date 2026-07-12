mod analysis;
mod clock;
mod collect;
mod data;
mod levels;
mod model;
mod sources;
mod tui;

use analysis::{day_key, median, Window};
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
    /// Measure real token-type weights (output vs input vs cache) from your data.
    Calibrate,
    /// Show how the exchange rate varies by time of day (mix-adjusted).
    Clock,
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
        Cmd::Calibrate => calibrate(),
        Cmd::Clock => clock(),
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

fn calibrate() -> Result<()> {
    let d = Dataset::load(Weights::default())?;
    let assumed = Weights::default();
    println!("\x1b[1mdrainage calibrate\x1b[0m  — measured token-type weights vs assumed");
    println!("─────────────────────────────────────────────");
    println!("levels regression of used% on cumulative RAW tokens per type\n");

    let Some(provider) = d.providers().into_iter().next() else {
        println!("no utilization data yet — keep using your agents.");
        return Ok(());
    };

    let mut printed = false;
    for window in [Window::FiveHour, Window::SevenDay] {
        let Some((c, epochs, obs)) = levels::calibrate(&d.events, &d.snaps, &provider, window) else {
            continue;
        };
        printed = true;
        println!("{provider} [{}]  ({epochs} epochs, {obs} observations)", window.label());
        // A collapsed input coefficient (too few epochs / collinearity) makes the
        // normalization meaningless — flag rather than print misleading weights.
        if epochs < 4 || c[0] < 0.01 {
            println!("  not enough independent signal to calibrate this window yet.\n");
            continue;
        }
        let base = c[0];
        println!("  cost of 1M input tokens ≈ {:.2}% of the {} window\n", c[0], window.label());
        println!("  {:<13} {:>10} {:>10}", "type", "measured", "assumed");
        let row = |name: &str, meas: f64, asmd: f64| {
            println!("  {name:<13} {:>10} {:>10}", format!("{:.2}", meas / base), format!("{asmd:.2}"));
        };
        row("input", c[0], assumed.input);
        row("output", c[1], assumed.output);
        row("cache_write", c[3], assumed.cache_write);
        row("cache_read", c[2], assumed.cache_read);
        println!();
    }
    if !printed {
        println!("not enough epochs yet to calibrate (need a few reset cycles of usage).");
    } else {
        println!("weights are relative to input=1. Token types are correlated, so a wobbly");
        println!("coefficient (esp. cache_read) usually means collinearity, not a real cost.");
    }
    Ok(())
}

fn clock() -> Result<()> {
    let d = Dataset::load(Weights::default())?;
    println!("\x1b[1mdrainage clock\x1b[0m  — exchange rate by time of day (mix-adjusted)");
    println!("─────────────────────────────────────────────");
    let Some(provider) = d.providers().into_iter().next() else {
        println!("no utilization data yet.");
        return Ok(());
    };
    // 5h has far more spans/day than 7d, so it carries the time-of-day signal.
    let window = Window::FiveHour;
    let by_hour = d.hour_multipliers(&provider, window);
    let all: Vec<f64> = by_hour.values().flatten().copied().collect();
    if all.len() < 6 {
        println!("not enough data yet to profile time-of-day (need more usage across hours).");
        return Ok(());
    }
    let mut all_sorted = all.clone();
    let baseline = median(&mut all_sorted); // ≈1 by construction; normalize to it
    println!(
        "timezone: local {}   ·   {} measurements   ·   baseline = 1.00 (rel to your own average)\n",
        clock::local_offset_label(),
        all.len()
    );

    // Median relative multiplier for an inclusive hour range.
    let rel_for = |lo: u32, hi: u32| -> (f64, usize) {
        let mut v: Vec<f64> = by_hour
            .iter()
            .filter(|(h, _)| **h >= lo && **h < hi)
            .flat_map(|(_, xs)| xs.iter().copied())
            .collect();
        let n = v.len();
        if n == 0 || baseline <= 0.0 {
            (1.0, 0)
        } else {
            (median(&mut v) / baseline, n)
        }
    };

    println!("part of day (local)      rel    n");
    for (lo, hi, name) in [
        (0u32, 6u32, "00–06  late night"),
        (6, 12, "06–12  morning"),
        (12, 18, "12–18  afternoon"),
        (18, 24, "18–24  evening"),
    ] {
        let (rel, n) = rel_for(lo, hi);
        if n == 0 {
            println!("  {name:<20} {:>6}  {n:>3}", "—");
            continue;
        }
        let tag = if rel > 1.12 {
            "burns FASTER"
        } else if rel < 0.88 {
            "slower / more usage"
        } else {
            ""
        };
        let bar = "█".repeat(((rel * 8.0).round() as usize).min(40));
        println!("  {name:<20} {rel:>5.2}x {n:>3}  {bar} {tag}");
    }

    println!("\nhourly (rel to baseline):");
    for h in 0..24u32 {
        if let Some(xs) = by_hour.get(&h) {
            if !xs.is_empty() {
                let mut v = xs.clone();
                let rel = median(&mut v) / baseline;
                let bar = "█".repeat(((rel * 8.0).round() as usize).min(40));
                println!("  {h:02}:00  {rel:>5.2}x  ({:>2})  {bar}", xs.len());
            }
        }
    }
    println!("\n  rel > 1 = the window drained faster than your model mix alone explains at that hour.");
    println!("  (mix-adjusted, so it isolates a time effect from which models you happened to run.)");
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

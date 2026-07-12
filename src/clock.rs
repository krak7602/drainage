//! Time-of-day analysis: does the exchange rate depend on the hour you use it?
//!
//! The trap is model mix: if you run more Opus at night, night looks costlier
//! for the wrong reason. So we don't bucket the raw rate — we bucket a
//! *mix-adjusted multiplier*: observed rate ÷ the rate expected from that span's
//! model composition (using the global per-model rates). A multiplier > 1 means
//! the window drained faster than your models alone explain at that hour.
//!
//! To beat the ~1% quantization, we build adaptive spans within each reset-epoch
//! that accumulate at least `MIN_DELTA` percentage points of movement before
//! measuring, and tag each span by the local-time hour of its midpoint.

use crate::analysis::Window;
use crate::levels::{reset, used, window_secs};
use crate::model::{Provider, TokenEvent, UtilSnapshot, Weights};
use chrono::{Local, TimeZone, Timelike};
use std::collections::BTreeMap;

/// Minimum used-% movement per span, so quantization (±0.5) is a small fraction.
const MIN_DELTA: f64 = 4.0;

/// Local UTC offset label, e.g. "+05:30" (IST), for display.
pub fn local_offset_label() -> String {
    match Local.timestamp_opt(0, 0).single() {
        Some(dt) => dt.format("%:z").to_string(),
        None => "?".to_string(),
    }
}

/// hour-of-day (0..23, local time) → mix-adjusted rate multipliers.
pub fn hour_multipliers(
    events: &[TokenEvent],
    snaps: &[UtilSnapshot],
    provider: &Provider,
    window: Window,
    rates: &BTreeMap<String, f64>,
    weights: &Weights,
) -> BTreeMap<u32, Vec<f64>> {
    let wsecs = window_secs(window);
    let mut pevents: Vec<&TokenEvent> = events
        .iter()
        .filter(|e| &e.provider == provider && e.ts > 0)
        .collect();
    pevents.sort_by_key(|e| e.ts);

    let mut epochs: BTreeMap<i64, Vec<&UtilSnapshot>> = BTreeMap::new();
    for s in snaps
        .iter()
        .filter(|s| &s.provider == provider && used(s, window).is_some() && reset(s, window).is_some())
    {
        epochs.entry(reset(s, window).unwrap()).or_default().push(s);
    }

    let mut out: BTreeMap<u32, Vec<f64>> = BTreeMap::new();
    for (reset_at, mut group) in epochs {
        if group.len() < 2 {
            continue;
        }
        group.sort_by_key(|s| s.ts);
        let epoch_start = reset_at - wsecs;
        let ev: Vec<&TokenEvent> = pevents
            .iter()
            .copied()
            .filter(|e| e.ts > epoch_start && e.ts <= reset_at)
            .collect();
        if ev.is_empty() {
            continue;
        }

        // Non-overlapping adaptive spans within the epoch.
        let mut i = 0;
        while i < group.len() {
            let u_i = used(group[i], window).unwrap();
            let mut j = i + 1;
            while j < group.len() && used(group[j], window).unwrap() - u_i < MIN_DELTA {
                j += 1;
            }
            if j >= group.len() {
                break;
            }
            let (t_a, t_b) = (group[i].ts, group[j].ts);
            let d_used = used(group[j], window).unwrap() - u_i;

            let mut tok: BTreeMap<&str, f64> = BTreeMap::new();
            for e in &ev {
                if e.ts > t_a && e.ts <= t_b {
                    let w = e.weighted(weights);
                    if w > 0.0 {
                        *tok.entry(e.model.as_str()).or_default() += w / 1_000_000.0;
                    }
                }
            }
            let total: f64 = tok.values().sum();
            // Only trust spans dominated by models we have a rate for.
            let rated: f64 = tok
                .iter()
                .filter(|(m, _)| rates.contains_key(**m))
                .map(|(_, t)| *t)
                .sum();
            if total > 0.0 && rated / total >= 0.8 {
                let observed = d_used / total;
                let exp_num: f64 = tok
                    .iter()
                    .map(|(m, t)| rates.get(*m).copied().unwrap_or(0.0) * t)
                    .sum();
                let expected = exp_num / total;
                if expected > 0.0 {
                    let mid = (t_a + t_b) / 2;
                    if let Some(dt) = Local.timestamp_opt(mid, 0).single() {
                        out.entry(dt.hour()).or_default().push(observed / expected);
                    }
                }
            }
            i = j;
        }
    }
    out
}

//! The core measurement: how much of a rate-limit window a token actually costs,
//! and how that "exchange rate" drifts over time.
//!
//! Method: between two consecutive utilization snapshots, the window's used-%
//! moved by Δpct. We attribute that to the (weighted) tokens spent in the same
//! interval and report Δpct per 1M weighted tokens. We only trust intervals
//! where Δpct > 0 (the window was filling, not decaying), and we segment by
//! model so that a shift in model mix or cache-hit rate doesn't masquerade as
//! genuine rate drift.

use crate::model::{Provider, TokenEvent, UtilSnapshot, Weights};
use chrono::{TimeZone, Utc};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    FiveHour,
    SevenDay,
}

impl Window {
    pub fn label(&self) -> &'static str {
        match self {
            Window::FiveHour => "5h",
            Window::SevenDay => "7d",
        }
    }
}

/// One measured exchange-rate observation over a time interval.
/// Some fields are kept for inspection/future detail views, not all are read yet.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Interval {
    pub provider: Provider,
    pub window: Window,
    pub t0: i64,
    pub t1: i64,
    pub delta_pct: f64,
    pub weighted: f64,
    pub raw: u64,
    pub output: u64,
    pub dominant_model: String,
    /// Δpct per 1M weighted tokens — the exchange rate.
    pub rate_per_mtok: f64,
}

pub struct Analysis {
    pub intervals: Vec<Interval>,
    /// Intervals skipped because the window decayed (Δpct ≤ 0) despite spend.
    pub decayed_skipped: usize,
}

fn dominant(events: &[&TokenEvent], w: &Weights) -> (String, f64) {
    use std::collections::HashMap;
    let mut by_model: HashMap<&str, f64> = HashMap::new();
    let mut total = 0.0;
    for e in events {
        let wt = e.weighted(w);
        *by_model.entry(e.model.as_str()).or_default() += wt;
        total += wt;
    }
    let best = by_model
        .into_iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    match best {
        Some((m, wt)) if total > 0.0 && wt / total >= 0.8 => (m.to_string(), total),
        _ => ("mixed".to_string(), total),
    }
}

pub fn analyze(events: &[TokenEvent], snaps: &[UtilSnapshot], w: &Weights) -> Analysis {
    let mut intervals = Vec::new();
    let mut decayed_skipped = 0;

    // Windows are scoped per subscription account/provider; token spend from ANY
    // harness on that provider draws down the same window.
    let providers: BTreeSet<Provider> = snaps.iter().map(|s| s.provider.clone()).collect();
    for provider in providers {
        let hs: Vec<&UtilSnapshot> = snaps.iter().filter(|s| s.provider == provider).collect();
        for pair in hs.windows(2) {
            let (s0, s1) = (pair[0], pair[1]);
            if s1.ts <= s0.ts {
                continue;
            }
            let in_interval: Vec<&TokenEvent> = events
                .iter()
                .filter(|e| e.provider == provider && e.ts > s0.ts && e.ts <= s1.ts)
                .collect();
            if in_interval.is_empty() {
                continue;
            }
            let weighted: f64 = in_interval.iter().map(|e| e.weighted(w)).sum();
            let raw: u64 = in_interval.iter().map(|e| e.raw_total()).sum();
            let output: u64 = in_interval.iter().map(|e| e.output).sum();
            let (model, _) = dominant(&in_interval, w);

            for (window, p0, p1) in [
                (Window::FiveHour, s0.five_pct, s1.five_pct),
                (Window::SevenDay, s0.week_pct, s1.week_pct),
            ] {
                let (Some(p0), Some(p1)) = (p0, p1) else {
                    continue;
                };
                let delta = p1 - p0;
                if delta <= 0.0 {
                    decayed_skipped += 1;
                    continue;
                }
                if weighted <= 0.0 {
                    continue;
                }
                intervals.push(Interval {
                    provider: provider.clone(),
                    window,
                    t0: s0.ts,
                    t1: s1.ts,
                    delta_pct: delta,
                    weighted,
                    raw,
                    output,
                    dominant_model: model.clone(),
                    rate_per_mtok: delta / (weighted / 1_000_000.0),
                });
            }
        }
    }

    Analysis {
        intervals,
        decayed_skipped,
    }
}

pub fn median(xs: &mut [f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

pub fn day_key(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "????-??-??".to_string())
}

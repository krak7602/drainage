//! The core measurement: how much of a rate-limit window a token actually costs,
//! per model, and how that "exchange rate" drifts over time.
//!
//! The exchange rate is intrinsically PER MODEL — Opus consumes far more of a
//! window per token than Sonnet, so a rate pooled across models just measures
//! your model mix, not anything real. So every `Interval` stores its full
//! per-model token vector (the regression "design row") alongside the observed
//! Δ%. Stage 1 attributes single-model-dominant intervals directly; later stages
//! (NNLS over windows, then a Kalman filter) consume the same rows to decompose
//! mixed intervals — no rework needed.

use crate::model::{Provider, TokenEvent, UtilSnapshot, Weights};
use chrono::{TimeZone, Utc};
use std::collections::{BTreeMap, BTreeSet};

/// A model must be at least this share of an interval's (weighted) spend to be
/// credited with that interval's Δ% under stage-1 single-model attribution.
pub const DOMINANT_SHARE: f64 = 0.9;

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
    pub fn other(&self) -> Window {
        match self {
            Window::FiveHour => Window::SevenDay,
            Window::SevenDay => Window::FiveHour,
        }
    }
}

/// One window's movement over one inter-snapshot interval, with the per-model
/// weighted-token breakdown that (plus unobserved account-global usage) produced
/// it. `tokens_by_model` × unknown per-model rates ≈ `delta_pct` is the
/// regression the later stages solve.
#[derive(Debug, Clone)]
pub struct Interval {
    pub provider: Provider,
    pub window: Window,
    pub t1: i64,
    pub delta_pct: f64,
    /// (model, weighted tokens) — weighted excludes cache-reads (weight 0), which
    /// are ~free against the limit, so a cache-heavy session isn't mispriced.
    pub tokens_by_model: Vec<(String, f64)>,
    pub total_weighted: f64,
}

impl Interval {
    /// The model with the largest share of this interval's spend, and its share.
    pub fn dominant(&self) -> Option<(&str, f64)> {
        if self.total_weighted <= 0.0 {
            return None;
        }
        let best = self
            .tokens_by_model
            .iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())?;
        Some((best.0.as_str(), best.1 / self.total_weighted))
    }
}

pub struct Analysis {
    pub intervals: Vec<Interval>,
    /// Intervals skipped because the window decayed (Δpct ≤ 0) despite spend.
    pub decayed_skipped: usize,
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
            let evs: Vec<&TokenEvent> = events
                .iter()
                .filter(|e| e.provider == provider && e.ts > s0.ts && e.ts <= s1.ts)
                .collect();
            if evs.is_empty() {
                continue;
            }
            let mut by_model: BTreeMap<String, f64> = BTreeMap::new();
            for e in &evs {
                *by_model.entry(e.model.clone()).or_default() += e.weighted(w);
            }
            let total_weighted: f64 = by_model.values().sum();
            if total_weighted <= 0.0 {
                continue;
            }
            let tokens_by_model: Vec<(String, f64)> = by_model.into_iter().collect();

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
                intervals.push(Interval {
                    provider: provider.clone(),
                    window,
                    t1: s1.ts,
                    delta_pct: delta,
                    tokens_by_model: tokens_by_model.clone(),
                    total_weighted,
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

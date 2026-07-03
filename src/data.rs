//! A loaded snapshot of everything drainage knows, plus the per-model
//! aggregations the CLI and TUI render. Reloadable, so the TUI can poll live.
//!
//! Stage 1 attribution: a model's exchange rate is the median, over intervals
//! where that model was ≥`DOMINANT_SHARE` of spend, of Δ% ÷ (weighted Mtok).
//! Mixed intervals are left unattributed (reported as a fraction) until the
//! NNLS/Kalman stages mine them.

use crate::analysis::{analyze, day_key, median, Analysis, Window, DOMINANT_SHARE};
use crate::model::{Harness, Provider, TokenEvent, UtilSnapshot, Weights};
use crate::sources;
use anyhow::Result;
use std::collections::BTreeMap;

pub struct ModelRow {
    pub harness: Harness,
    pub provider: Provider,
    pub model: String,
    pub raw: u64,
    pub output: u64,
    pub weighted: f64,
    pub calls: u64,
}

pub struct Dataset {
    pub events: Vec<TokenEvent>,
    pub snaps: Vec<UtilSnapshot>,
    pub weights: Weights,
    pub analysis: Analysis,
    pub n_claude: usize,
    pub n_codex: usize,
    pub n_omp: usize,
    pub n_claude_snaps: usize,
    pub n_codex_snaps: usize,
    pub n_omp_snaps: usize,
}

impl Dataset {
    pub fn load(weights: Weights) -> Result<Self> {
        let mut events = sources::claude::token_events()?;
        let claude_snaps = sources::claude::util_snapshots()?;
        let codex_events = sources::codex::token_events()?;
        let codex_snaps = sources::codex::util_snapshots()?;
        let omp_events = sources::omp::token_events()?;
        let omp_snaps = sources::omp::util_snapshots()?;

        let n_claude = events.len();
        let n_codex = codex_events.len();
        let n_omp = omp_events.len();
        let n_claude_snaps = claude_snaps.len();
        let n_codex_snaps = codex_snaps.len();
        let n_omp_snaps = omp_snaps.len();

        events.extend(codex_events);
        events.extend(omp_events);
        let mut snaps = claude_snaps;
        snaps.extend(codex_snaps);
        snaps.extend(omp_snaps);
        snaps.sort_by_key(|s| s.ts);

        let analysis = analyze(&events, &snaps, &weights);
        Ok(Self {
            events,
            snaps,
            weights,
            analysis,
            n_claude,
            n_codex,
            n_omp,
            n_claude_snaps,
            n_codex_snaps,
            n_omp_snaps,
        })
    }

    /// Distinct subscription accounts (providers) we have utilization for,
    /// busiest first (so the busiest is "primary").
    pub fn providers(&self) -> Vec<Provider> {
        let mut counts: BTreeMap<Provider, usize> = BTreeMap::new();
        for s in &self.snaps {
            *counts.entry(s.provider.clone()).or_default() += 1;
        }
        let mut v: Vec<(Provider, usize)> = counts.into_iter().collect();
        v.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        v.into_iter().map(|(p, _)| p).collect()
    }

    pub fn latest_snap(&self, provider: &Provider) -> Option<&UtilSnapshot> {
        self.snaps.iter().rev().find(|s| &s.provider == provider)
    }

    /// Per-model spend rows for the attribution table, weighted-desc.
    pub fn by_model(&self) -> Vec<ModelRow> {
        let mut acc: BTreeMap<(Harness, String), ModelRow> = BTreeMap::new();
        for e in &self.events {
            let row = acc
                .entry((e.harness, e.model.clone()))
                .or_insert_with(|| ModelRow {
                    harness: e.harness,
                    provider: e.provider.clone(),
                    model: e.model.clone(),
                    raw: 0,
                    output: 0,
                    weighted: 0.0,
                    calls: 0,
                });
            row.raw += e.raw_total();
            row.output += e.output;
            row.weighted += e.weighted(&self.weights);
            row.calls += 1;
        }
        let mut rows: Vec<ModelRow> = acc.into_values().collect();
        rows.sort_by(|a, b| b.weighted.partial_cmp(&a.weighted).unwrap());
        rows
    }

    /// Models for a provider (from any harness), by weighted spend desc.
    pub fn models(&self, provider: &Provider) -> Vec<String> {
        let mut acc: BTreeMap<String, f64> = BTreeMap::new();
        for e in self.events.iter().filter(|e| &e.provider == provider) {
            *acc.entry(e.model.clone()).or_default() += e.weighted(&self.weights);
        }
        let mut v: Vec<(String, f64)> = acc.into_iter().collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        v.into_iter().map(|(m, _)| m).collect()
    }

    /// Stage-1 exchange rate for one model+window: median Δ% per weighted Mtok
    /// over intervals that model dominated. Returns (rate, n_intervals).
    pub fn model_rate(&self, provider: &Provider, model: &str, window: Window) -> Option<(f64, usize)> {
        let mut rates: Vec<f64> = self
            .single_model_intervals(provider, model, window)
            .map(|i| i.delta_pct / (i.total_weighted / 1_000_000.0))
            .collect();
        if rates.is_empty() {
            None
        } else {
            let n = rates.len();
            Some((median(&mut rates), n))
        }
    }

    /// Per-day median rate for one model+window — the drift series to chart.
    pub fn model_drift_series(&self, provider: &Provider, model: &str, window: Window) -> Vec<(f64, f64)> {
        let mut per_day: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut day_ts: BTreeMap<String, i64> = BTreeMap::new();
        for i in self.single_model_intervals(provider, model, window) {
            let k = day_key(i.t1);
            per_day
                .entry(k.clone())
                .or_default()
                .push(i.delta_pct / (i.total_weighted / 1_000_000.0));
            day_ts.entry(k).or_insert(i.t1);
        }
        let mut out: Vec<(f64, f64)> = per_day
            .into_iter()
            .map(|(k, mut rs)| (day_ts[&k] as f64, median(&mut rs)))
            .collect();
        out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        out
    }

    /// (recent median, older median, % change) for one model+window.
    pub fn model_drift_summary(
        &self,
        provider: &Provider,
        model: &str,
        window: Window,
    ) -> Option<(f64, f64, f64)> {
        let series = self.model_drift_series(provider, model, window);
        if series.len() < 2 {
            return None;
        }
        let mid = series.len() / 2;
        let mut older: Vec<f64> = series[..mid].iter().map(|p| p.1).collect();
        let mut recent: Vec<f64> = series[mid..].iter().map(|p| p.1).collect();
        let o = median(&mut older);
        let r = median(&mut recent);
        let change = if o > 0.0 { (r - o) / o * 100.0 } else { 0.0 };
        Some((r, o, change))
    }

    /// (attributed weighted tokens, unattributed/mixed weighted tokens) for a
    /// window — so we can honestly report how much spend stage 1 can't yet price.
    pub fn attribution_coverage(&self, provider: &Provider, window: Window) -> (f64, f64) {
        let mut attributed = 0.0;
        let mut mixed = 0.0;
        for i in self
            .analysis
            .intervals
            .iter()
            .filter(|i| &i.provider == provider && i.window == window)
        {
            match i.dominant() {
                Some((_, share)) if share >= DOMINANT_SHARE => attributed += i.total_weighted,
                _ => mixed += i.total_weighted,
            }
        }
        (attributed, mixed)
    }

    fn single_model_intervals<'a>(
        &'a self,
        provider: &'a Provider,
        model: &'a str,
        window: Window,
    ) -> impl Iterator<Item = &'a crate::analysis::Interval> {
        self.analysis.intervals.iter().filter(move |i| {
            &i.provider == provider
                && i.window == window
                && i.total_weighted > 0.0
                && matches!(i.dominant(), Some((m, share)) if m == model && share >= DOMINANT_SHARE)
        })
    }
}

//! A loaded snapshot of everything drainage knows, plus the aggregations the
//! CLI and the TUI both render. Reloadable, so the TUI can poll for live data.

use crate::analysis::{analyze, day_key, median, Analysis, Window};
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
    /// Median 5h exchange rate when this model dominated an interval.
    pub rate_5h: Option<f64>,
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
    /// ordered by snapshot count desc so the busiest is "primary".
    pub fn providers(&self) -> Vec<Provider> {
        let mut counts: BTreeMap<Provider, usize> = BTreeMap::new();
        for s in &self.snaps {
            *counts.entry(s.provider.clone()).or_default() += 1;
        }
        let mut v: Vec<(Provider, usize)> = counts.into_iter().collect();
        v.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        v.into_iter().map(|(p, _)| p).collect()
    }

    /// Per-model spend + exchange rate, sorted by weighted spend desc.
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
                    rate_5h: None,
                });
            row.raw += e.raw_total();
            row.output += e.output;
            row.weighted += e.weighted(&self.weights);
            row.calls += 1;
        }
        let mut rows: Vec<ModelRow> = acc.into_values().collect();
        for row in &mut rows {
            let mut rates: Vec<f64> = self
                .analysis
                .intervals
                .iter()
                .filter(|i| {
                    i.provider == row.provider
                        && i.window == Window::FiveHour
                        && i.dominant_model == row.model
                })
                .map(|i| i.rate_per_mtok)
                .collect();
            if !rates.is_empty() {
                row.rate_5h = Some(median(&mut rates));
            }
        }
        rows.sort_by(|a, b| b.weighted.partial_cmp(&a.weighted).unwrap());
        rows
    }

    /// Per-day median exchange rate for a window — the drift series for charts.
    pub fn drift_series(&self, provider: &Provider, window: Window) -> Vec<(f64, f64)> {
        let mut per_day: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut day_ts: BTreeMap<String, i64> = BTreeMap::new();
        for i in self
            .analysis
            .intervals
            .iter()
            .filter(|i| &i.provider == provider && i.window == window)
        {
            let k = day_key(i.t1);
            per_day.entry(k.clone()).or_default().push(i.rate_per_mtok);
            day_ts.entry(k).or_insert(i.t1);
        }
        let mut out: Vec<(f64, f64)> = per_day
            .into_iter()
            .map(|(k, mut rs)| (day_ts[&k] as f64, median(&mut rs)))
            .collect();
        out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        out
    }

    pub fn latest_snap(&self, provider: &Provider) -> Option<&UtilSnapshot> {
        self.snaps.iter().rev().find(|s| &s.provider == provider)
    }

    /// Overall median exchange rate for a window (across all models).
    pub fn median_rate(&self, provider: &Provider, window: Window) -> Option<f64> {
        let mut rates: Vec<f64> = self
            .analysis
            .intervals
            .iter()
            .filter(|i| &i.provider == provider && i.window == window)
            .map(|i| i.rate_per_mtok)
            .collect();
        if rates.is_empty() {
            None
        } else {
            Some(median(&mut rates))
        }
    }

    /// Drift readout: (recent median, older median, pct change) for a window,
    /// splitting the drift series at its midpoint in time.
    pub fn drift_summary(&self, provider: &Provider, window: Window) -> Option<(f64, f64, f64)> {
        let series = self.drift_series(provider, window);
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
}

//! Stage 3: levels-based state-space estimator.
//!
//! The utilization %s are ~1%-quantized and reset on fixed epoch boundaries
//! (verified empirically: within an epoch used% only rises, modulo ±1 jitter).
//! So instead of *differencing* the quantized signal (stages 1–2), we regress
//! its LEVELS: within each reset-epoch,
//!
//!     used%(t)  ≈  Σ_model  rate_model · cumulative_weighted_tokens_model(t)
//!
//! a non-negative, through-origin fit (used%=0 at the reset). Quantization is
//! just ±0.5 noise on each of the ~20 observations per epoch, so the slope
//! (the rate) is well determined. Each epoch yields one robust rate per model;
//! the sequence of epochs is the drift signal, which a scalar Kalman filter then
//! smooths into a time-varying estimate.

use crate::analysis::{nnls, Window};
use crate::model::{Provider, TokenEvent, UtilSnapshot, Weights};
use std::collections::{BTreeMap, BTreeSet};

pub const FIVE_H_SECS: i64 = 5 * 3600;
pub const SEVEN_D_SECS: i64 = 7 * 86_400;

pub(crate) fn window_secs(w: Window) -> i64 {
    match w {
        Window::FiveHour => FIVE_H_SECS,
        Window::SevenDay => SEVEN_D_SECS,
    }
}
pub(crate) fn used(s: &UtilSnapshot, w: Window) -> Option<f64> {
    match w {
        Window::FiveHour => s.five_pct,
        Window::SevenDay => s.week_pct,
    }
}
pub(crate) fn reset(s: &UtilSnapshot, w: Window) -> Option<i64> {
    match w {
        Window::FiveHour => s.five_reset,
        Window::SevenDay => s.week_reset,
    }
}

/// One epoch's rate measurement for a model: (epoch end ts, rate %/Mtok, #obs).
pub type EpochPoint = (i64, f64, usize);

/// Per-model sequence of per-epoch levels-NNLS rate measurements (chronological).
pub fn epoch_rates(
    events: &[TokenEvent],
    snaps: &[UtilSnapshot],
    provider: &Provider,
    window: Window,
    weights: &Weights,
) -> BTreeMap<String, Vec<EpochPoint>> {
    let wsecs = window_secs(window);
    let mut pevents: Vec<&TokenEvent> = events
        .iter()
        .filter(|e| &e.provider == provider && e.ts > 0)
        .collect();
    pevents.sort_by_key(|e| e.ts);

    // Group snapshots into epochs by their reset boundary.
    let mut epochs: BTreeMap<i64, Vec<&UtilSnapshot>> = BTreeMap::new();
    for s in snaps
        .iter()
        .filter(|s| &s.provider == provider && used(s, window).is_some() && reset(s, window).is_some())
    {
        epochs.entry(reset(s, window).unwrap()).or_default().push(s);
    }

    let mut out: BTreeMap<String, Vec<EpochPoint>> = BTreeMap::new();
    for (reset_at, mut group) in epochs {
        if group.len() < 3 {
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
        let mut modset: BTreeSet<String> = BTreeSet::new();
        for e in &ev {
            if e.weighted(weights) > 0.0 {
                modset.insert(e.model.clone());
            }
        }
        let models: Vec<String> = modset.into_iter().collect();
        if models.is_empty() {
            continue;
        }
        let idx: BTreeMap<&str, usize> =
            models.iter().enumerate().map(|(i, m)| (m.as_str(), i)).collect();
        let ncols = models.len();

        // Each snapshot is one observation: cumulative per-model tokens vs used%.
        let mut rows: Vec<(Vec<f64>, f64)> = Vec::with_capacity(group.len());
        for s in &group {
            let u = used(s, window).unwrap();
            let mut cum = vec![0.0; ncols];
            for e in &ev {
                if e.ts <= s.ts {
                    if let Some(&j) = idx.get(e.model.as_str()) {
                        cum[j] += e.weighted(weights) / 1_000_000.0;
                    }
                }
            }
            rows.push((cum, u));
        }
        // Skip barely-used epochs (no signal above the quantization floor).
        if rows.iter().map(|(_, u)| *u).fold(0.0, f64::max) < 2.0 {
            continue;
        }
        let x = nnls(ncols, &rows, 1e-6);
        let n = group.len();
        for (m, r) in models.into_iter().zip(x) {
            out.entry(m).or_default().push((reset_at, r, n));
        }
    }
    for v in out.values_mut() {
        v.sort_by_key(|p| p.0);
    }
    out
}

/// Calibrate token-type weights from data: regress used% LEVELS on cumulative
/// RAW tokens of each type (input / output / cacheRead / cacheWrite), pooled
/// across epochs (all pass through the origin at reset). Returns the fitted
/// per-type cost `[input, output, cache_read, cache_write]` in %/Mtok, plus the
/// epoch and observation counts. This measures the *real* weights — e.g. whether
/// output really costs ~5× input and whether cache reads are ~free.
pub fn calibrate(
    events: &[TokenEvent],
    snaps: &[UtilSnapshot],
    provider: &Provider,
    window: Window,
) -> Option<([f64; 4], usize, usize)> {
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

    let mut rows: Vec<(Vec<f64>, f64)> = Vec::new();
    let mut n_epochs = 0;
    for (reset_at, mut group) in epochs {
        if group.len() < 3 {
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
        if group.iter().filter_map(|s| used(s, window)).fold(0.0, f64::max) < 2.0 {
            continue;
        }
        n_epochs += 1;
        for s in &group {
            let u = used(s, window).unwrap();
            let mut cum = [0.0f64; 4];
            for e in &ev {
                if e.ts <= s.ts {
                    cum[0] += e.input as f64;
                    cum[1] += e.output as f64;
                    cum[2] += e.cache_read as f64;
                    cum[3] += e.cache_write as f64;
                }
            }
            rows.push((vec![cum[0] / 1e6, cum[1] / 1e6, cum[2] / 1e6, cum[3] / 1e6], u));
        }
    }
    if rows.len() < 8 || n_epochs == 0 {
        return None;
    }
    let x = nnls(4, &rows, 1e-6);
    Some(([x[0], x[1], x[2], x[3]], n_epochs, rows.len()))
}

/// Scalar Kalman filter over one model's epoch-rate measurements. State = the
/// (slowly drifting) true rate; random-walk process. Returns the filtered
/// trajectory as (epoch end ts, smoothed rate, posterior variance). `Q`/`R` are
/// deliberately simple, tunable constants: `Q` allows drift between epochs,
/// measurement variance shrinks with the number of observations per epoch.
pub fn kalman(meas: &[EpochPoint]) -> Vec<(f64, f64, f64)> {
    if meas.is_empty() {
        return Vec::new();
    }
    const Q: f64 = 0.5; // process noise (%/Mtok)² per epoch — how fast rate may drift
    const R_BASE: f64 = 6.0; // measurement noise scale; divided by #obs
    let mut x = meas[0].1;
    let mut p = 10.0_f64; // initial state variance
    let mut out = Vec::with_capacity(meas.len());
    for &(t, z, n) in meas {
        let r = R_BASE / (n as f64).max(1.0);
        p += Q; // predict
        let k = p / (p + r); // Kalman gain
        x += k * (z - x); // update
        p *= 1.0 - k;
        out.push((t as f64, x, p));
    }
    out
}

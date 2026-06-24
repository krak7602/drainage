//! Core data types shared across sources and analysis.

use std::fmt;

/// Which coding tool produced the spend. Used for attribution only — NOT for
/// scoping rate-limit windows (see `Provider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Harness {
    ClaudeCode,
    Codex,
    Omp,
}

impl fmt::Display for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Codex => "codex",
            Harness::Omp => "omp",
        };
        write!(f, "{s}")
    }
}

/// The subscription account a request draws down. Rate-limit windows are scoped
/// HERE, not by harness: Claude Code and omp-on-Anthropic share one Anthropic
/// window; Codex and omp-on-OpenAI share one OpenAI window.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    Anthropic,
    OpenAI,
    Other(String),
}

impl Provider {
    /// Map a harness/provider string to an account scope.
    pub fn classify(s: &str) -> Provider {
        let l = s.to_ascii_lowercase();
        if l.contains("anthropic") || l.contains("claude") {
            Provider::Anthropic
        } else if l.contains("openai") || l.contains("codex") || l.contains("chatgpt") || l.contains("gpt") {
            Provider::OpenAI
        } else {
            Provider::Other(l)
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Provider::Anthropic => write!(f, "anthropic"),
            Provider::OpenAI => write!(f, "openai"),
            Provider::Other(s) => write!(f, "{s}"),
        }
    }
}

/// One billable inference response: typed token counts at a point in time.
#[derive(Debug, Clone)]
pub struct TokenEvent {
    pub ts: i64, // unix seconds
    pub harness: Harness,
    pub provider: Provider,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    /// Stable identity to dedup the same response logged in multiple files.
    #[allow(dead_code)]
    pub id: String,
}

impl TokenEvent {
    pub fn raw_total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write + self.reasoning
    }

    /// "Weighted" tokens: a transparent model of how much a token costs against
    /// the limit. These weights are an ASSUMPTION, not ground truth — the whole
    /// point of drainage is to measure how the real cost drifts away from them.
    pub fn weighted(&self, w: &Weights) -> f64 {
        self.input as f64 * w.input
            + self.output as f64 * w.output
            + self.cache_read as f64 * w.cache_read
            + self.cache_write as f64 * w.cache_write
            + self.reasoning as f64 * w.reasoning
    }
}

/// Default weights reflect documented rough ratios (output ~5x input, cache
/// reads ~free against limits, cache writes a bit above input). Configurable so
/// the user can calibrate against observed exchange rates.
#[derive(Debug, Clone, Copy)]
pub struct Weights {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub reasoning: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            input: 1.0,
            output: 5.0,
            cache_read: 0.0,
            cache_write: 1.25,
            reasoning: 5.0,
        }
    }
}

/// A reading of how full the rate-limit windows are for one account at a moment.
#[derive(Debug, Clone)]
pub struct UtilSnapshot {
    pub ts: i64,
    pub provider: Provider,
    /// Which source observed it (statusline, rollout file, omp db) — for debug.
    #[allow(dead_code)]
    pub source: Harness,
    pub five_pct: Option<f64>,
    pub week_pct: Option<f64>,
    pub five_reset: Option<i64>,
    pub week_reset: Option<i64>,
}

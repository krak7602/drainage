# drainage

**Track the drifting "exchange rate" between tokens spent and the percentage of your AI-subscription usage limit they consume — and watch how it changes over time.**

Per token, the share of your 5-hour / 7-day limit that gets consumed is **not constant**. It varies with the model (Opus burns far more of your window than Sonnet), your cache-hit rate, provider policy changes, and rate-card updates. For the *same* person on the *same* subscription, the rate drifts week to week. drainage measures that drift — locally, from data your coding agents already write to disk.

```
drainage           # interactive TUI (Drift · Attribution · Budget)
drainage scan      # one-shot text report
drainage clock     # how the rate varies by time of day (mix-adjusted)
drainage calibrate # measure your real token-type weights from data
drainage init      # install the collector into your Claude Code statusline
```

Estimates carry a confidence glyph (●●● / ●●○ / ●○○) from sample size and Kalman
uncertainty, and the levels estimate shows ±σ, so you can see how much to trust
a number while data is still thin.

## Why this is hard (and how drainage does it)

You need two signals joined over time:

1. **Token spend** — how many (typed) tokens you burned, by model and time.
2. **Limit utilization** — what % of your 5h / 7d window was consumed at that moment.

The catch: utilization is reported *live* and, for Claude Code, **is never written to disk** — so it can't be reconstructed after the fact. drainage installs a tiny statusline wrapper that captures it going forward. Token spend is read from existing transcripts.

| Harness | Token spend | Utilization % | Backfillable? |
|---|---|---|---|
| **Claude Code** | `~/.claude/projects/**/*.jsonl` | statusline `rate_limits` (captured by `drainage init`) | tokens yes, utilization no |
| **Codex** (ChatGPT sub) | `~/.codex/sessions/**/rollout-*.jsonl` | `codex.rate_limits` events in the same files | yes |
| **oh-my-pi** (`omp`) | `~/.omp/agent/sessions/**/*.jsonl` | SQLite `~/.omp/agent/agent.db` → `usage_history` | yes |

No proxy required. Everything stays on your machine.

## Install

```bash
cargo install --git https://github.com/krak7602/drainage   # or download a release binary
drainage init                                               # wire up the collector (Claude Code)
```

`init` is safe and reversible: it backs up `~/.claude/settings.json`, **preserves your existing statusline** (drainage wraps it — logs a snapshot, then renders your line unchanged), and `drainage uninstall` restores it.

Then just use Claude Code as normal. Snapshots accumulate at `~/.drainage/claude_ratelimit.jsonl`; the exchange rate sharpens over a few days.

## How the measurement works

Between two utilization snapshots, the window's used-% moved by Δ. drainage attributes that to the tokens spent in the interval and reports **Δ% per 1M weighted tokens** — the exchange rate. Then it tracks how that number drifts.

**The rate is strictly per-model.** Opus consumes far more of a window per token than Sonnet, so a rate pooled across models just measures your model *mix*, not anything real. Rates are reported per (account, model, window) — never pooled. This is the same decomposition used in hedonic regression, spectral unmixing, and energy disaggregation (NILM). Three estimators are built in (toggle with `m`):

1. **Single-model** — a model's rate from intervals where it was ≥90% of spend. Transparent, but biased *high*: utilization % is ~1%-quantized, so a short interval only registers when a tiny spend crosses a 1% boundary, overstating the rate.
2. **NNLS-delta** — non-negative least squares over per-interval deltas; decomposes mixed intervals but inherits the same quantization bias.
3. **levels + Kalman** *(default)* — the fix. The windows are fixed-reset epochs where used% accumulates, so instead of *differencing* the quantized signal, we regress its **levels**: within each epoch, `used%(t) = Σ_model rate_model · cumulative_tokens_model(t)`, a non-negative through-origin fit. Quantization becomes ±0.5 noise over ~20 observations per epoch. Each epoch yields one robust rate; a scalar **Kalman filter** over the epoch sequence tracks the drift.

Guards against false drift:

- **Account-scoped windows.** Limits are scoped to the *subscription account*, not the tool. Claude Code and omp-on-Anthropic pool against one Anthropic window; Codex and omp-on-OpenAI pool against the OpenAI window.
- **Token weighting.** Output ≈ 5× input, cache writes a bit above input, and **cache reads are excluded** (≈ free against the limit) — so a change in your cache-hit rate doesn't look like rate drift. Weights are a transparent assumption you can calibrate.
- **Decay skipping.** The 5h window is rolling; intervals where it was draining (not filling) are excluded.

## Honest caveats

- Utilization is **account-global**: usage from claude.ai chat or other sessions on the same account is invisible here and adds noise.
- The default token weights are assumptions, not ground truth — the tool measures drift *relative* to them until you calibrate.
- Claude Code utilization only exists from the moment you run `drainage init` — it can't be backfilled.

## Privacy

100% local. drainage reads files your agents already write and never sends anything anywhere.

## License

MIT

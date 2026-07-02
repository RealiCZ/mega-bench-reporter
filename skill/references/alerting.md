# When an alert fires

The tool decides this itself; the relaying agent only relays. The rules:

- A **headline-family row** whose ratio rises **more than `regression_threshold_pct`** (default 10%) **above its own rolling median** (median of its last `rolling_window` healthy runs, default 20) → `regression_alert` card, the same run it happens.
- The alert is **latched**: no repeat cards while the row stays regressed; one `recovery` card when it drops back under.
- The baseline is **frozen while a row is regressed** — regressed values never enter the rolling window, so a sustained regression stays latched against the pre-regression baseline instead of silently becoming the new normal. To deliberately accept a new performance level, delete that row's entry from `state.json` (it re-baselines on the next run).
- Re-running the sha recorded as `last_seen_sha` refreshes artifacts but does not touch the window or the digest counter (retry-safe).
- First run ever (no history) establishes the baseline and never alerts.
- Every `digest_batch_size` commits (default 10) → `trend_digest` card, possibly alongside an alert card in the same run's `cards[]`. If the digest build fails (e.g. no headline rows yet), the counter is not reset and it retries next commit.

## Manual verification

To sanity-check a claimed regression by hand: the authoritative window is `state.json → rows.<row_key>.recent_ratios` (regressed-era values are excluded); check `current > median(recent_ratios) * (1 + threshold/100)`.

The trend chart's red rings are a window-local approximation (each point vs the median of the points before it inside the digest window) — a visual cue, not the live check.

## Configuration

`regression_threshold_pct`, `rolling_window`, `digest_batch_size`, and `bench_profile` live in the config file (`repos.toml`): a `[defaults]` section plus optional per-repo overrides inside each `[[repos]]` entry.
Built-in fallbacks (used when neither is set) are in `src/state.rs`.
The comparison table's color bands (≤1.075× green, ≤1.25× amber, >1.25× red) are fixed in `src/charts.rs`.

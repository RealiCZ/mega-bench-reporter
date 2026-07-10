# Events — the facts a run can emit

`<commit_dir>/events.json` is an array of factual events; the stdout summary echoes
the same list. The reporter decides *what happened*; what to do about it (post, page,
ignore) is the consumer's call. A missing `events.json` means no events were recorded
for that dir — treat as `[]`.

## Event types

```json
{ "type": "regression", "row_key": "salt_dynamic_gas/rex5_salt/sstore_100",
  "baseline_median": 2.0, "current": 2.3, "pct_over": 15.0 }

{ "type": "regression", "row_key": "salt_dynamic_gas/rex5_salt/sstore_100",
  "baseline_median": 2.0, "current": 2.06, "pct_over": 3.0, "metric": "instructions" }

{ "type": "recovery", "row_key": "salt_dynamic_gas/rex5_salt/sstore_100",
  "baseline_median": 2.0, "current": 2.02 }

{ "type": "digest", "dir": "digests/20260702-abc1234..def5678" }
```

- `regression` — a headline row's `ratio_vs_baseline` rose more than
  `regression_threshold_pct` (default 10%) above the median of its own last
  `rolling_window` (default 20) healthy runs. Fires **once** per regression: the row
  is latched and stays silent until it recovers.
- `recovery` — a latched row dropped back within `recovery_threshold_pct` of its
  frozen median (defaults to the regression threshold; configure it lower for
  hysteresis so a row oscillating around the regression threshold stays latched
  and quiet in between). Fires once.
- `digest` — every `digest_batch_size` (default 10) commits; `dir` (repo-relative)
  holds `summary.json` + `trend.png` for the window.
- `metric` — which lane a regression/recovery came from: **absent = walltime**
  (unchanged from before the field existed), `"instructions"` = the
  instruction-count lane. Instructions events use the same latch protocol but
  their own state (`state.json → instr_rows`), their own thresholds
  (`instr_regression_threshold_pct`, default 2%, and
  `instr_recovery_threshold_pct`), and ratios over deterministic counts — so any
  instructions regression is a real code-path change, not noise.

## Semantics that matter when interpreting

- Only **headline-family** subjects (config `headline_subjects`, e.g. `rex5`,
  `rex5_*`) can emit regression/recovery; every row's history is still recorded.
- The baseline is **frozen while a row is regressed** — regressed values never enter
  the rolling window, so a sustained regression cannot silently become the new
  normal. Accepting a new performance level = the `rebaseline` subcommand
  (see `cli.md`), which clears the row's entry so it re-baselines on the next run.
- First run ever (no history) establishes baselines and emits nothing.
- A failed digest build (e.g. no headline rows yet) is retried on the next commit;
  the event only appears when the digest actually materialized.
- To verify a regression by hand: `state.json → rows.<row_key>.recent_ratios` is the
  authoritative window; check `current > median(recent_ratios) * (1 + threshold/100)`.
  For instructions events use `instr_rows.<row_key>` and the instr thresholds instead.

## Tuning

`regression_threshold_pct`, `recovery_threshold_pct`,
`instr_regression_threshold_pct`, `instr_recovery_threshold_pct`,
`rolling_window`, `digest_batch_size`, `bench_profile` live in `repos.toml`
(`[defaults]` + per-repo overrides); built-in fallbacks in `src/config.rs`.

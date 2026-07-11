# Events — the facts a run can emit

`<commit_dir>/events.json` is an array of factual events; the stdout summary echoes
the same list. The reporter decides *what happened*; what to do about it (post, page,
ignore) is the consumer's call. A missing `events.json` means no events were recorded
for that dir — treat as `[]`.

## Event types

```json
{ "type": "regression", "row_key": "salt_dynamic_gas/rex5_salt/sstore_100",
  "baseline_median": 2.0, "current": 2.3, "pct_over": 15.0,
  "instructions": { "ratio_delta_pct": -0.31, "verdict": "flat" } }

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
  holds `summary.json` + `trend.png` (plus the instructions lane's
  `instr_trend.png` when the window has instructions data, see
  [`data-layout.md`](data-layout.md)) for the window.
- `metric` — which lane a regression/recovery came from: **absent = walltime**
  (unchanged from before the field existed), `"instructions"` = the
  instruction-count lane. Instructions events use the same latch protocol but
  their own state (`state.json → instr_rows`), their own thresholds
  (`instr_regression_threshold_pct`, default 2%, and
  `instr_recovery_threshold_pct`), and ratios over deterministic counts:
  instruction counts are byte-identical across repeat runs on the same commit/host,
  so a latched instructions regression is a real code-path change, not machine
  noise — however small the percentage.
- `instructions` — optional, on **walltime** regression/recovery events only:
  what the instructions lane saw for the same row when the walltime alert
  fired. `verdict` is `"flat" | "up" | "down" | "missing"`. `"missing"` (with
  `ratio_delta_pct: null`) = the row has no instructions data this run or no
  instructions rolling median yet. Otherwise `ratio_delta_pct` =
  `(current_instr_ratio / instr_rolling_median - 1) * 100` — the same
  pre-update median the instructions lane's own check compares against — and
  the verdict is `"up"` if `ratio_delta_pct >= instr_regression_threshold_pct`,
  `"down"` if `<= -instr_regression_threshold_pct`, else `"flat"`. The verdict is
  computed from the exact delta **before** the 2-decimal rounding of the serialized
  `ratio_delta_pct`, so an exact-boundary case may show e.g.
  `ratio_delta_pct: 2.0, verdict: "flat"`. A walltime
  regression with instructions `"flat"` is likely machine noise or a layout
  effect; with `"up"` it is corroborated by a real code-path change (card
  wording in [`lark-card.md`](lark-card.md)).

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

# Data layout on disk

Everything is under `<data-root>/<repo>/`:

| path | what it is |
|---|---|
| `latest.json` | pointer to the newest completed run: `{sha, commit_dir, finished_at}` (see [`discovery.md`](discovery.md)) |
| `commits/<YYYYMMDD>-<shortsha>/raw.json` | source of truth for one benched commit: every row's mean ns and ratio, plus instruction counts when that lane ran |
| `commits/<YYYYMMDD>-<shortsha>/events.json` | the run's factual events (see [`events.md`](events.md)); missing = no events recorded |
| `commits/<YYYYMMDD>-<shortsha>/compare_table.json` | table-ready JSON: `{subjects[], headline_label, baseline_subject, rows[{item, p95_us[], headline_ratio, instr[]?, instr_headline_ratio?}]}` (`p95_us` aligns with `subjects`; `null` = subject absent; `headline_ratio` = worst headline time ratio; the optional `instr` fields mirror that for instruction counts) |
| `commits/<YYYYMMDD>-<shortsha>/compare_bars.png` | grouped bars: relative speed per item, baseline = 100% (lower = more overhead) |
| `commits/<YYYYMMDD>-<shortsha>/instr_bars.png` | grouped bars: relative instruction count per item, baseline = 100% — written only when the commit has instructions data |
| `commits/<YYYYMMDD>-<shortsha>/dist_<group>[_<workload>].png` | violin plot of per-call time distributions (`/` in workloads becomes `_`) |
| `digests/<YYYYMMDD>-<first>..<last>/summary.json` | last-N-commits headline series: per-row `ratios[]`, `first`, `last`, `median`; the optional `instr_series` mirrors it for the instructions lane |
| `digests/<YYYYMMDD>-<first>..<last>/trend.png` | headline ratios over the digest window, red rings on threshold-tripping points |
| `digests/<YYYYMMDD>-<first>..<last>/instr_trend.png` | instructions ratios over the digest window, same chart style; red rings on instructions-threshold-tripping points; commits without instructions data are gaps — written only when the window has instructions data |
| `trends/<YYYYMMDD>-<first>..<last>/` | manual `trend` runs — same `summary.json` + `trend.png` shape as a digest (with `--metric instructions`: `instr_trend.png` + `summary.json` with `instr_series`), never produced automatically |
| `flame/<YYYYMMDD>/<workload>.svg` | flame graph of one benchmark id (nightly, archive-only — open directly in a browser) |
| `flame/<YYYYMMDD>/<workload>_diff.svg` | differential flame graph, feature vs baseline (red = grew, blue = shrank) |
| `state.json` | rolling windows, regression latches, digest counter, `last_seen_sha` |

The commit-dir date is the commit's committer date, not the run date.

## raw.json schema

```json
{
  "commit": "<full sha>",
  "date": "<committer date, RFC3339>",
  "rustc": "rustc 1.x.y (…)",
  "baseline_subject": "revm_pinned",
  "failed_targets": ["block_bench"],
  "instr_failed_targets": ["comp_cost"],
  "groups": {
    "<group>": {
      "<subject>[/<workload>]": {
        "ns": 24899.0,
        "ratio_vs_baseline": 1.76,
        "instr": { "count": 105230, "ratio_vs_baseline": 1.62 }
      }
    }
  }
}
```

- `ns` — mean wall-clock per call, nanoseconds; lower is faster.
- `ratio_vs_baseline` — this row's `ns` / the `baseline_subject` row's `ns` for the
  same group/workload. **1.0 = as fast as the baseline; 2.0 = twice as slow; <1.0 =
  faster.** `null` = no baseline row for that group/workload.
- `failed_targets` present only when non-empty: those targets' rows are absent, the
  rest of the data is still valid.
- `instr` — the instructions lane's numbers, present only when that lane ran and
  produced a count for the row: `count` = CPU instructions retired (callgrind `Ir`)
  for one traced iteration, `ratio_vs_baseline` = same semantics as the walltime
  ratio but over counts. Counts are **deterministic** (byte-identical across
  repeat runs on the same commit/host).
- `instr_failed_targets` — bench targets whose instructions-lane build/run failed;
  absent when the lane is off, skipped, or fully clean. Independent of
  `failed_targets` (the walltime marker).
- `p95_us` (compare_table.json) is the 95th percentile of per-call times in µs —
  more outlier-sensitive than the mean; ratios still use means.
- `instr` / `instr_headline_ratio` (compare_table.json, optional): per-subject
  instruction counts aligned with `subjects[]`, and the worst headline-family
  count ratio for the item. Absent on rows without instructions data.

## summary.json schema (digest)

```json
{
  "commits": ["<oldest sha>", "…", "<newest sha>"],
  "first_commit": "…", "last_commit": "…",
  "rows": [
    { "row_key": "salt_dynamic_gas/rex5_salt/sstore_100",
      "ratios": [2.0, null, 2.1], "first": 2.0, "last": 2.1, "median": 2.05 }
  ],
  "failed_targets": []
}
```

Rows are headline-family only, sorted by median ratio descending. `null` in
`ratios` = the row was missing that run.

`instr_series` (optional) — the instructions lane's counterpart to `rows`:
same structure, but each `ratios` value is that commit's instructions
`ratio_vs_baseline`, `null` for commits without instructions data. Its chart
is `instr_trend.png` beside `trend.png` — same style, red rings on
instructions-threshold-tripping points, `null` commits drawn as gaps.

## state.json semantics

`rows.<row_key>.recent_ratios` is the rolling window (healthy runs only);
`currently_regressed` is the event latch; `commits_since_digest` counts toward the
next digest; `last_seen_sha` powers the retry-idempotence guard.
`instr_rows` (present only once the instructions lane has recorded something) is
the same structure for that lane — same row keys, independent windows and latches.

Do not hand-edit. Accepting a new performance level for a row (clearing its window +
latch — in both lanes — so it re-baselines next run) is the `rebaseline` subcommand's
job — see [`cli.md`](cli.md) — and only ever follows an explicit human decision.

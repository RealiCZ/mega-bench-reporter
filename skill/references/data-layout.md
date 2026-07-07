# Data layout on disk

Everything is under `<data-root>/<repo>/`:

| path | what it is |
|---|---|
| `latest.json` | pointer to the newest completed run: `{sha, commit_dir, finished_at}` (see [`discovery.md`](discovery.md)) |
| `commits/<YYYYMMDD>-<shortsha>/raw.json` | source of truth for one benched commit: every row's mean ns and ratio |
| `commits/<YYYYMMDD>-<shortsha>/events.json` | the run's factual events (see [`events.md`](events.md)); missing = no events recorded |
| `commits/<YYYYMMDD>-<shortsha>/compare_table.json` | table-ready JSON: `{subjects[], headline_label, baseline_subject, rows[{item, p95_us[], headline_ratio}]}` (`p95_us` aligns with `subjects`; `null` = subject absent; `headline_ratio` = worst headline time ratio) |
| `commits/<YYYYMMDD>-<shortsha>/compare_bars.png` | grouped bars: relative speed per item, baseline = 100% (lower = more overhead) |
| `commits/<YYYYMMDD>-<shortsha>/dist_<group>[_<workload>].png` | violin plot of per-call time distributions (`/` in workloads becomes `_`) |
| `digests/<YYYYMMDD>-<first>..<last>/summary.json` | last-N-commits headline series: per-row `ratios[]`, `first`, `last`, `median` |
| `digests/<YYYYMMDD>-<first>..<last>/trend.png` | headline ratios over the digest window, red rings on threshold-tripping points |
| `trends/<YYYYMMDD>-<first>..<last>/` | manual `trend` runs — same `summary.json` + `trend.png` shape as a digest, never produced automatically |
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
  "groups": {
    "<group>": {
      "<subject>[/<workload>]": { "ns": 24899.0, "ratio_vs_baseline": 1.76 }
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
- `p95_us` (compare_table.json) is the 95th percentile of per-call times in µs —
  more outlier-sensitive than the mean; ratios still use means.

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

## state.json semantics

`rows.<row_key>.recent_ratios` is the rolling window (healthy runs only);
`currently_regressed` is the event latch; `commits_since_digest` counts toward the
next digest; `last_seen_sha` powers the retry-idempotence guard.

Do not hand-edit. Accepting a new performance level for a row (clearing its window +
latch so it re-baselines next run) is the `rebaseline` subcommand's job — see
[`cli.md`](cli.md) — and only ever follows an explicit human decision.

# Data layout on disk

Everything is under `<data-root>/<repo>/` (repo = `mega-evm`):

| path | what it is |
|---|---|
| `commits/<YYYYMMDD>-<shortsha>/raw.json` | source of truth for one benched commit: every row's mean ns and ratio |
| `commits/<YYYYMMDD>-<shortsha>/compare_table.json` | table-ready JSON: `{subjects[], headline_label, rows[{item, p95_us[], headline_ratio}]}` — build a native Lark table from it (`p95_us` aligns with `subjects`; `null` = subject absent; `headline_ratio` = worst headline-family time ratio, >1 = slower than revm) |
| `commits/<YYYYMMDD>-<shortsha>/compare_bars.png` | grouped bars: relative speed per item, revm_pinned = 100% (lower = more overhead) |
| `commits/<YYYYMMDD>-<shortsha>/dist_<group>[_<workload>].png` | violin plot of per-call time distributions (`/` in workloads becomes `_`) |
| `digests/<YYYYMMDD>-<first>..<last>/summary.json` | last-N-commits headline series: per-row `ratios[]`, `first`, `last`, `median` |
| `digests/<YYYYMMDD>-<first>..<last>/trend.png` | headline ratios over the digest window, red rings on threshold-tripping points |
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
  "failed_targets": ["block_bench"],
  "groups": {
    "<group>": {
      "<subject>[/<workload>]": { "ns": 24899.0, "ratio_vs_revm_pinned": 1.76 }
    }
  }
}
```

`failed_targets` is present only when non-empty. `ratio_vs_revm_pinned` is `null` when the group/workload has no `revm_pinned` baseline row.

## summary.json schema

```json
{
  "commits": ["<oldest sha>", "…", "<newest sha>"],
  "first_commit": "…", "last_commit": "…",
  "rows": [
    { "row_key": "salt_dynamic_gas/rex5_salt/sstore_100",
      "ratios": [2.0, null, 2.1, "…"], "first": 2.0, "last": 2.1, "median": 2.05 }
  ],
  "failed_targets": []
}
```

Rows are headline-family only, sorted by median ratio descending. `null` in `ratios` = the row was missing that run.

## state.json semantics

`rows.<row_key>.recent_ratios` is the rolling window (healthy runs only — regressed values are excluded); `currently_regressed` is the alert latch; `commits_since_digest` counts toward the next digest; `last_seen_sha` powers the retry-idempotence guard.

Do not hand-edit `state.json` — with one exception: deleting a single row's entry is the sanctioned way to accept a new performance level (the row re-baselines on the next run).
Deleting the whole file resets every row to "first run" (no alert on the first post-reset run; windows rebuild over the following runs).

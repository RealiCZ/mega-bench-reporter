---
name: mega-bench-data
description: Use when operating mega-bench-reporter or consuming its output - triggering benchmark runs on new mega-evm commits, relaying its Lark cards, reading benchmark data under the data root (raw.json, summary.json, state.json, charts, flame graphs), interpreting ratio_vs_revm_pinned numbers, or deciding whether a regression alert is warranted.
---

# mega-bench-reporter: driving it and reading its data

## What this tool is

`mega-bench-reporter` measures how much slower mega-evm is than vanilla revm, per commit, and renders ready-to-post Lark cards.
It never talks to Lark itself: you (the relaying agent) invoke the CLI, read one JSON document from stdout, upload attachments, and post the card.

## When and how to invoke it

On every new commit on a tracked branch (poll with `git ls-remote`, compare against `state.json`'s `last_seen_sha`):

```bash
mega-bench-reporter run --repo mega-evm --sha <full-sha> \
  --config repos.toml --data-root <data-root>
```

Nightly (Linux box only, needs `perf`):

```bash
mega-bench-reporter flamegraph --repo mega-evm \
  --config repos.toml --data-root <data-root>
```

- Exit code 0 = success; stdout carries exactly one JSON document; stderr is logs.
- A run takes as long as the benches take (tens of minutes) — do not add short timeouts.
- Set `GITHUB_TOKEN` in the environment only if the tracked repo is private.
- Never run two invocations for the same repo concurrently: they share the checkout and `state.json`.

## Relaying the output

Output shape:

```json
{
  "repo": "mega-evm",
  "sha": "...",
  "output_dir": ".../commits/20260702-d21a86f",
  "failed_targets": [],
  "cards": [ { "kind": "...", "card": {...}, "attachments": ["..."] } ]
}
```

- `cards` empty → nothing to post; you are done.
- For each card: upload every attachment an `img` element references, then string-replace `${image:<basename>}` in the card JSON with the returned Lark `image_key`, then post the card JSON as an interactive card.
- `kind` values: `regression_alert` (red, post immediately), `recovery` (green), `trend_digest` (blue, every 10th commit), `flamegraph` (orange, nightly).
- Flamegraph attachments are SVGs — Lark cards cannot embed them; post them as plain file messages in the same thread as the card.
- `failed_targets` non-empty means some bench targets failed but the run still produced data; mention it when relaying, do not treat it as a failed run.

## Where the data lives

Everything is under `<data-root>/<repo>/` (repo = `mega-evm`):

| Path | What it is |
| --- | --- |
| `commits/<YYYYMMDD>-<shortsha>/raw.json` | Source of truth for one benched commit: every row's mean ns and ratio. |
| `commits/<YYYYMMDD>-<shortsha>/compare.png` | Bar chart of headline-family ratios (one bar per workload/subject). |
| `commits/<YYYYMMDD>-<shortsha>/dist_<group>[_<workload>].png` | Violin plot of per-call time distributions for that group/workload. |
| `digests/<YYYYMMDD>-<first>..<last>/summary.json` | Last-10-commits headline series: per-row `ratios[]`, `first`, `last`, `median`. |
| `digests/<YYYYMMDD>-<first>..<last>/trend.png` | Headline ratios over the 10-commit window. |
| `flame/<YYYYMMDD>/<workload>.svg` | Flame graph of one benchmark id (nightly). |
| `flame/<YYYYMMDD>/<workload>_diff.svg` | Differential flame graph, feature vs baseline (red = grew, blue = shrank). |
| `state.json` | Rolling medians, regression latches, digest counter, `last_seen_sha`. |

Do not hand-edit `state.json`; deleting it resets all baselines to "first run" (no alerts until history rebuilds).

## What the numbers mean

`raw.json.groups` is `{ <group>: { <subject>[/<workload>]: { ns, ratio_vs_revm_pinned } } }`.

- `ns` — mean wall-clock time per call, nanoseconds. Lower is faster.
- `ratio_vs_revm_pinned` — this row's `ns` divided by the `revm_pinned` row's `ns` for the same group/workload. **1.0 = as fast as vanilla revm; 2.0 = twice as slow; below 1.0 = faster.** `null` = that group/workload has no `revm_pinned` baseline row to compare against.

Subjects (the row names):

- `revm_pinned` — vanilla revm at the version mega-evm builds on. **The baseline every ratio is against.**
- `revm_latest`, `op_revm_pinned`, `op_revm_latest` — upstream reference rows; context, never alerted on.
- `equivalence`, `mini_rex`, `rex4`, `rex5` — mega-evm at each spec; the gap over `revm_pinned` is mega-evm's overhead at that spec.
- `rex5_salt` — rex5 with a crowded SALT external environment (real bucket-multiplier work on storage writes); the `rex5_salt` − `rex5` gap isolates the SALT dynamic-gas path cost.
- `rex5_oracle` (and `rex4_oracle`) — rex5/rex4 with populated oracle storage; measures the oracle SLOAD hit path.

Groups worth knowing: `salt_dynamic_gas` (SSTORE/CREATE under SALT pricing), `oracle_real_data` (oracle SLOAD), `empty_transaction` (fixed per-tx overhead), `sstore_heavy`, `volatile_data` (gas-detention paths), plus the `comp_cost` precompile groups.

The **headline family** is the configured `headline_spec` (currently `rex5`) plus its `_`-suffixed variants (`rex5_salt`, `rex5_oracle`).
Headline rows drive alerts, the compare chart, and digests; everything else is recorded for history/context.

## When an alert is warranted

The tool decides this itself; you only relay. The rules it applies:

- A **headline-family row** whose ratio rises **more than 10% above its own rolling median** (median of its last 20 runs) → `regression_alert` card, the same run it happens.
- The alert is **latched**: no repeat cards while the row stays regressed; one `recovery` card when it drops back under.
- First run ever (no history) establishes the baseline and never alerts.
- Every 10th commit → `trend_digest` card regardless of regressions.

If you need to sanity-check a claimed regression by hand: take the row's `ratios` history from the recent `commits/*/raw.json`, compute the median of the last 20 values before the flagged run, and check `current > median * 1.10`.
Thresholds live in `src/state.rs` (`REGRESSION_THRESHOLD_PCT`, `ROLLING_WINDOW`, `DIGEST_BATCH_SIZE`) — changing them is a code change in this repo, not a config knob.

## Card templates

The three card layouts are JSON files in `templates/`:

- `templates/regression_alert.json` — red regression alert; the same layout renders green recovery cards (color/title are parameters).
- `templates/trend_digest.json` — blue 10-commit digest: summary, markdown table, trend chart.
- `templates/flamegraph.json` — orange nightly flamegraph card; SVGs ride as file attachments, not embedded images.

Template language (all of it): `{{key}}` substitution inside string values, plus `{"tag": "__images__", "group": "<name>"}` marker elements that expand to one `img` element per image.
`img` elements carry `${image:<basename>}` placeholders you replace after uploading the attachment.
To change a card's look, edit the template and run `cargo test` (each template has rendering tests); the binary embeds templates at compile time, so rebuild after editing.

---
name: mega-bench-data
description: Use when operating mega-bench-reporter or consuming its output - triggering benchmark runs on new commits, discovering new results via latest.json, interpreting events (regression/recovery/digest) and ratio_vs_baseline numbers, composing Lark cards from the run data, or reading anything under the data root (raw.json, compare_table.json, summary.json, state.json, charts, flame graphs).
---

# mega-bench-data — driving mega-bench-reporter and consuming its data

`mega-bench-reporter` continuously measures a repo's benchmark overhead against a
configured baseline (for mega-evm: vanilla `revm_pinned`) and produces **data only**:
raw metrics JSON, charts, and factual events. It never talks to Lark and renders no
cards — discovering results, composing cards, and delivering them is entirely the
consuming agent's job, following these docs.

## Commands

| command | purpose |
|---|---|
| `mega-bench-reporter run --repo <name> --sha <sha> --config repos.toml --data-root <dir>` | per-commit pipeline: bench → parse → charts + table JSON → store → events (regression/recovery/digest) |
| `mega-bench-reporter flamegraph --repo <name> --config repos.toml --data-root <dir>` | nightly flame-graph archive into `flame/<day>/` — no events, nothing to relay |
| `mega-bench-reporter trend --repo <name> --config repos.toml --data-root <dir> [--last N \| --from <sha> --to <sha>] [--row <key>]...` | manual trend chart over any stored-commit window into `trends/` — read-only, independent of the automatic digest |
| `mega-bench-reporter rebaseline --repo <name> --data-root <dir> --row <key-or-prefix*>...` | accept a latched regression as the new normal: clear the rows' history + latch from `state.json`; next run re-baselines them (no alert). Only on an explicit human decision |

Ground rules: exit 0 = success; stdout = one JSON summary (facts are durable on disk
regardless); stderr = logs; runs take tens of minutes — no short timeouts; a per-repo
lock rejects concurrent invocations; `GITHUB_TOKEN` only for private repos.

## Routing — which doc for what

| you are… | read |
|---|---|
| invoking the CLI (flags, env, stdout shape, locking, failure modes) | [`references/cli.md`](references/cli.md) |
| finding new results, deduping ("did I already post this sha?"), recovering after a crash | [`references/discovery.md`](references/discovery.md) |
| interpreting events (when a regression fires, latch semantics, tuning knobs) | [`references/events.md`](references/events.md) |
| locating and decoding files (raw.json / compare_table.json / summary.json / state.json / charts) | [`references/data-layout.md`](references/data-layout.md) |
| composing a Lark card from a run (field mapping, color standard, skeleton, gold example) | [`references/lark-card.md`](references/lark-card.md) |
| what mega-evm's subjects/groups mean (`rex5_salt`, `oracle_real_data`, …) | [`references/repos/mega-evm.md`](references/repos/mega-evm.md) |

Adding a tracked repo touches no code: a new `[[repos]]` entry in `repos.toml`
(baseline_subject, headline_subjects, bench targets) plus a
`references/repos/<name>.md` describing its subjects.

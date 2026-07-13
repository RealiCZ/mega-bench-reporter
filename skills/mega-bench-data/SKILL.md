---
name: mega-bench-data
description: Use when operating mega-bench-reporter or consuming its output - triggering benchmark runs on new commits, discovering new results via latest.json, interpreting events (regression/recovery/digest) and ratio_vs_baseline numbers, composing Lark cards from the run data, or reading anything under the data root (raw.json, compare_table.json, summary.json, state.json, charts, flame graphs).
---

# mega-bench-data — driving mega-bench-reporter and consuming its data

`mega-bench-reporter` continuously measures a repo's benchmark overhead against a
configured baseline (for mega-evm: vanilla `revm_pinned`) and produces **data only**:
raw metrics JSON, charts, and factual events. Two metric lanes exist: walltime
(always on) and deterministic CPU instruction counts (opt-in per repo, Linux-only;
events from it carry `"metric": "instructions"`). It never talks to Lark and renders
no cards — discovering results, composing cards, and delivering them is entirely the
consuming agent's job, following these docs.

## Commands

| subcommand | purpose |
|---|---|
| `run` | per-commit pipeline: bench → parse → charts + table JSON → store → events (regression/recovery/digest) |
| `trend` | manual trend chart over any stored-commit window into `trends/` — either lane, read-only, independent of the automatic digest |
| `rebaseline` | accept a latched regression as the new normal: clear the rows' history + latch from `state.json`; next run re-baselines them (no alert). Only on an explicit human decision |
| `flamegraph` | nightly flame-graph archive into `flame/<day>/` — no events, nothing to relay |
| `measure` | one-shot metric collection on an arbitrary checkout (walltime and/or instructions); single JSON on stdout; primary consumer is the ARO optimization loop's terminal gate — see [`references/cli.md`](references/cli.md#measure-one-shot-metrics-on-an-existing-checkout) |

```bash
mega-bench-reporter run --repo <name> --sha <sha> --config repos.toml --data-root <dir>
```

Ground rules before any invocation: the reporter produces **data only** — it never
posts, pages, or renders cards; that stays the consumer's job. Exit 0 = success,
except `require_instructions = true`, which turns a skipped/failed instructions lane
into a nonzero exit after the walltime data has landed. Full flags, env, stdout,
locking, and safety rules: [`references/cli.md`](references/cli.md).

## Routing — which doc for what

| you are… | read |
|---|---|
| invoking any subcommand — run / trend / rebaseline / flamegraph / measure (flags, env, stdout, locking, failure modes) | [`references/cli.md`](references/cli.md) |
| one-shot metrics on a caller-owned checkout (ARO terminal gate, walltime / instructions lanes) | [`references/cli.md`](references/cli.md#measure-one-shot-metrics-on-an-existing-checkout) |
| finding new results, deduping ("did I already post this sha?"), recovering after a crash | [`references/discovery.md`](references/discovery.md) |
| interpreting events (when a regression fires, latch semantics, tuning knobs) | [`references/events.md`](references/events.md) |
| locating and decoding files (raw.json / compare_table.json / summary.json / state.json / charts) | [`references/data-layout.md`](references/data-layout.md) |
| composing a Lark card from a run (field mapping, color standard, skeleton, gold example) | [`references/lark-card.md`](references/lark-card.md) |
| what mega-evm's subjects/groups mean (`rex5_salt`, `oracle_real_data`, …) | [`references/repos/mega-evm.md`](references/repos/mega-evm.md) |

Adding a tracked repo touches no code: a new `[[repos]]` entry in `repos.toml`
(baseline_subject, headline_subjects, bench targets) plus a
`references/repos/<name>.md` describing its subjects.

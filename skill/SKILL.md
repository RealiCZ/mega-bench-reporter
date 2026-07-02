---
name: mega-bench-data
description: Use when operating mega-bench-reporter or consuming its output - triggering benchmark runs on new mega-evm commits, relaying its Lark cards, reading benchmark data under the data root (raw.json, summary.json, state.json, charts, flame graphs), interpreting ratio_vs_revm_pinned numbers, or deciding whether a regression alert is warranted.
---

# mega-bench-data — driving mega-bench-reporter and reading its data

`mega-bench-reporter` measures how much slower mega-evm is than vanilla revm, per commit, and renders ready-to-post Lark cards.
It never talks to Lark itself: the relaying agent (e.g. BB9) invokes the CLI, reads one JSON document from stdout, uploads attachments, and posts the cards.

## Commands

| command | purpose |
|---|---|
| `mega-bench-reporter run --repo <name> --sha <sha> --config repos.toml --data-root <dir>` | per-commit pipeline: bench → charts → store → regression check → (every Nth commit) trend digest |
| `mega-bench-reporter run … --skip-bench` | re-render charts/records from the checkout's existing criterion tree (dev/regen; no benching) |
| `mega-bench-reporter flamegraph --repo <name> --config repos.toml --data-root <dir>` | nightly flame-graph **archive**: profile configured workloads (Linux `perf` / macOS `sample`), render SVG + differential into `flame/<day>/` — no cards, nothing to relay |

Ground rules: exit 0 = success; stdout carries exactly one JSON document; stderr is logs; runs take as long as the benches take (tens of minutes — no short timeouts); a per-repo lock rejects concurrent invocations; set `GITHUB_TOKEN` only for private repos.

## Routing — which doc for what

| you are… | read |
|---|---|
| invoking the CLI (flags, env, locking, exit codes, failure modes) | [`references/cli.md`](references/cli.md) |
| relaying output to Lark (stdout JSON shape, `${image:}` placeholders, SVG handling, card kinds) | [`references/output-contract.md`](references/output-contract.md) |
| finding data on disk (directory tree, raw.json / summary.json / state.json schemas) | [`references/data-layout.md`](references/data-layout.md) |
| interpreting the numbers (ns, p95, ratio_vs_revm_pinned, subjects glossary, groups, headline family) | [`references/metrics.md`](references/metrics.md) |
| understanding when an alert fires (threshold, rolling median, latch, recovery, digest cadence, config knobs) | [`references/alerting.md`](references/alerting.md) |
| changing what a card looks like (templates, template language, chart set per card) | [`references/cards.md`](references/cards.md) |

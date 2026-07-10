# Invoking the CLI

## Per-commit run

```bash
mega-bench-reporter run --repo <name> --sha <full-sha> \
  --config repos.toml --data-root <data-root>
```

What it does: clone/fetch the repo into `<work-root>/<repo>` (default
`<data-root>/_checkouts`), check out the sha (submodules included), run
`cargo bench -p <package> --bench <target> -- --output-format bencher` per configured
target (the invocation the scheduled walltime layer standardized on; a
`bench_profile` config adds `--profile <p>`), parse criterion's JSON tree, compute
ratios against the configured `baseline_subject`, render charts + table JSON,
record events, update `state.json` and `latest.json`.

- A failing bench target lands in `failed_targets` and is skipped; the run only
  fails when every target fails.
- With `[repos.instructions]` configured, the instructions lane runs after the
  walltime benches on the same checkout: per target, an instrumented build
  (`cargo codspeed build`) plus an offline `codspeed run --skip-upload --mode
  simulation`, parsed into per-row instruction counts. Linux-only (valgrind);
  on other hosts, or when the `codspeed` CLI / `cargo-codspeed` are missing, the
  lane is skipped with a stderr note and the run proceeds walltime-only. A
  lane-failing target lands in `instr_failed_targets`; the lane never fails the
  run.
- No need to hold a live connection: launch detached, wait for exit, read the files
  (see [`discovery.md`](discovery.md)).
- `--skip-bench` re-renders artifacts from the checkout's existing criterion tree —
  dev/regen only; it accepts only the last processed sha, and it never re-collects
  the instructions lane (the regenerated raw.json is walltime-only).

## stdout summary

Exactly one JSON document (logs go to stderr); the same facts are durable on disk:

```json
{
  "repo": "mega-evm",
  "sha": "<full sha>",
  "output_dir": "<data-root>/mega-evm/commits/20260702-d21a86f",
  "failed_targets": [],
  "events": [ { "type": "regression", "row_key": "…", "baseline_median": 2.0,
                "current": 2.3, "pct_over": 15.0 } ]
}
```

`instr_failed_targets` appears next to `failed_targets` only when the
instructions lane ran and some target failed; instructions-lane events carry
`"metric": "instructions"` (see [`events.md`](events.md)).

## Manual trend chart (read-only)

```
mega-bench-reporter trend --repo <name> --config repos.toml --data-root <dir> \
    [--last N] [--from <sha-prefix>] [--to <sha-prefix>] [--row <key>]... [--out <dir>]
```

Charts an arbitrary window of **already-stored** commits — nothing is benched,
no state or events are touched, and the automatic digest counter is unaffected.
Use it to answer "show me the last 30 commits" or "how has this one row moved"
without waiting for the next digest.

- Window: the most recent `--last` N records (default 20), or an explicit
  inclusive `--from`/`--to` sha-prefix range (overrides `--last`; either end
  may be omitted).
- Rows: defaults to the configured headline family; `--row` (repeatable,
  exact key or trailing `*`, e.g. `--row 'salt_dynamic_gas/*'`) charts any
  stored row instead, including non-headline ones.
- Output: `summary.json` + `trend.png` (same shape as a digest) under
  `<data-root>/<repo>/trends/<day>-<first7>..<last7>/`, or `--out <dir>`.
- stdout: one JSON document — `{repo, output_dir, commits, rows}`.
- Needs no lock and is safe to run while a bench run is in progress.

## Rebaseline (accept a regressed level as the new normal)

```bash
mega-bench-reporter rebaseline --repo <name> --data-root <dir> \
    --row <key-or-prefix*> [--row ...]
```

A sustained regression stays latched forever (frozen baseline, quiet after the
one alert). When the team decides the new level is acceptable, clear the
affected rows: their rolling history and regression latch are removed from
`state.json`, and the next run re-baselines them (FirstRun — no alert, fresh
window at the new level). A matching pattern clears the row from **both**
lanes — the walltime history (`rows`) and the instructions history
(`instr_rows`) — so an accepted new level never leaves a stale latch in the
other lane.

- `--row` is required and repeatable: exact row key or trailing `*`
  (e.g. `--row 'salt_dynamic_gas/rex5_salt/*'`). A pattern matching nothing
  is an error — check the row keys in `state.json`.
- Needs no config file; it operates on the stored state only.
- Takes the per-repo lock (it mutates `state.json`) — don't run mid-bench.
- stdout: one JSON document — `{repo, cleared: [<row keys>]}`.

## Nightly flamegraph (archive only)

```bash
mega-bench-reporter flamegraph --repo <name> --config repos.toml --data-root <data-root>
```

Checks out the tracked branch's HEAD, builds the bench binary once
(`cargo bench --no-run --profile profiling`), verifies each configured benchmark id
against `--list`, profiles each unique id (Linux: `perf`; macOS: the built-in
`sample` tool — no root), folds + demangles via `inferno`, renders one SVG per
workload plus one differential SVG per pair, prunes days past retention. No events,
nothing to relay — plain cron is enough; view `flame/<day>/*.svg` in a browser.

## Environment and safety

- Exit 0 = success. Runs take as long as the benches take (tens of minutes) — no
  short timeouts.
- `GITHUB_TOKEN` env var: only needed for private repos (https clone URLs use it via
  a git credential helper; the token never appears in argv).
- Concurrency: a per-repo lock (`<data-root>/<repo>/.lock`) makes a second
  invocation fail fast. Never run two invocations for the same repo at once.
- The box needs: `git`, a Rust toolchain, whatever the tracked repo's build needs
  (mega-evm: Foundry), `perf` on Linux for flamegraphs, and — for the instructions
  lane — the `codspeed` CLI, `cargo-codspeed`, and valgrind (`codspeed setup`),
  all host-provisioned; the reporter never installs them, it skips the lane when
  they are missing.

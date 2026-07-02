# Invoking the CLI

## Per-commit run

On every new commit on a tracked branch (poll with `git ls-remote`, compare against `state.json`'s `last_seen_sha`):

```bash
mega-bench-reporter run --repo mega-evm --sha <full-sha> \
  --config repos.toml --data-root <data-root>
```

What it does: clone/fetch the repo into `<work-root>/<repo>` (default `<data-root>/_checkouts`), check out the sha (submodules included), run `cargo bench -p <package> --bench <target> -- --output-format bencher` per configured target (the exact invocation mega-evm's CI uses; a `bench_profile` config adds `--profile <p>`), parse criterion's JSON tree, render charts, store the record, check regressions, and batch digests.

- A failing bench target is recorded in `failed_targets` and skipped; the run only fails when every target fails.
- No need to hold a live connection for the whole run: launch detached, wait for process exit, then read the durable `cards.json` in the commit dir (see output-contract.md for the recovery rules).
- Re-running the sha in `last_seen_sha` refreshes artifacts without touching regression state (retry-safe).
- `--skip-bench` reuses the checkout's existing criterion tree — for re-rendering charts after a template/chart change, not for production runs. It only accepts the last processed sha (`state.json`'s `last_seen_sha`); anything else is rejected because the tree's provenance is unknown.

## Nightly flamegraph (archive only)

```bash
mega-bench-reporter flamegraph --repo mega-evm --config repos.toml --data-root <data-root>
```

Checks out the tracked branch's HEAD, builds the bench binary once (`cargo bench --no-run --profile profiling`), verifies each configured benchmark id against `--list`, profiles each unique id (Linux: `perf record` + `perf script`; macOS: the built-in `sample` tool at 1 ms — no root needed), folds + demangles via `inferno`, renders one SVG per workload and one differential SVG per baseline/feature pair, prunes days past retention.

Pure archive: no card is produced (`cards` is always `[]`), so any timer works — plain cron on the box is enough, no relaying agent involvement needed. View the SVGs by opening `flame/<day>/*.svg` in a browser (they are interactive).

## Environment and safety

- Exit code 0 = success. stdout = exactly one JSON document. stderr = all logs and subprocess output.
- Runs take as long as the benches take (tens of minutes) — do not add short timeouts.
- `GITHUB_TOKEN` env var: set only for private repos; https clone URLs then authenticate via a git credential helper (token never in argv). Public repos need nothing.
- Concurrency: a per-repo lock (`<data-root>/<repo>/.lock`) makes a second invocation fail fast instead of corrupting shared state. Never run two invocations for the same repo at once.
- The machine needs: `git`, a Rust toolchain, whatever the tracked repo's build needs (mega-evm: Foundry), and `perf` (Linux) or nothing extra (macOS) for flamegraphs.

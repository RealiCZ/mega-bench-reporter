# Invoking the CLI

## Per-commit run

```bash
mega-bench-reporter run --repo <name> --sha <full-sha> \
  --config repos.toml --data-root <data-root>
```

What it does: clone/fetch the repo into `<work-root>/<repo>` (default
`<data-root>/_checkouts`), check out the sha (submodules included), run
`cargo bench -p <package> --bench <target> -- --output-format bencher` per configured
target (the exact invocation the tracked repo's CI uses; a `bench_profile` config adds
`--profile <p>`), parse criterion's JSON tree, compute ratios against the configured
`baseline_subject`, render charts + table JSON, record events, update `state.json`
and `latest.json`.

- A failing bench target lands in `failed_targets` and is skipped; the run only
  fails when every target fails.
- No need to hold a live connection: launch detached, wait for exit, read the files
  (see [`discovery.md`](discovery.md)).
- `--skip-bench` re-renders artifacts from the checkout's existing criterion tree —
  dev/regen only; it accepts only the last processed sha.

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
  (mega-evm: Foundry), and `perf` on Linux for flamegraphs.

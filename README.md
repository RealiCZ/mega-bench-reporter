# mega-bench-reporter

Continuous benchmark-overhead tracking for Rust repos, against a configured baseline
(for mega-evm: vanilla `revm_pinned`).
This tool owns "a commit landed" → "structured data on disk": clone/pull → `cargo bench` → parse → ratios → charts → categorized storage → regression detection → digest batching.
It produces **data only** — raw metrics JSON, chart images, and factual events.
It never calls the Lark API, renders no cards, and holds no messaging credentials: a consuming agent (e.g. BB9) discovers results from the data root and composes/delivers whatever reports it wants, guided by [`skill/`](skill/SKILL.md).

## Requirements

- `git` and a Rust toolchain (`cargo`, `rustc`) — the tool shells out to both to build and bench the tracked repo.
- Whatever the tracked repo itself needs to build (for mega-evm: git submodules are handled automatically, and Foundry must be installed).
- For the `flamegraph` subcommand: `perf` on Linux; on macOS the built-in `sample` tool is used (nothing to install).
- No Python, no gnuplot, no system fonts: charts are rendered with `plotters` using an embedded font, flame graphs with the `inferno` crate as a library.

## Build

```bash
cargo build --release
# single binary: target/release/mega-bench-reporter
```

## Usage

### Per-commit pipeline

```bash
mega-bench-reporter run \
  --repo mega-evm \
  --sha <full-commit-sha> \
  --config repos.toml \
  --data-root /srv/mega-bench/data
```

What one `run` does:

1. Clones (first run) or fetches the tracked repo into `<work-root>/<repo>` (default work root: `<data-root>/_checkouts`) and checks out the sha, submodules included.
2. Runs `cargo bench -p <package> --bench <target> -- --output-format bencher` for every configured bench target — the exact invocation mega-evm's CI (benchmark.yml) uses, so numbers stay comparable with the per-PR `/benchmark` flow (`bench_profile` in the config adds `--profile <p>`). A failing target is recorded in `failed_targets` and skipped; the run only fails when every target fails.
3. Parses criterion's `target/criterion/**/new/*.json` tree and computes each row's `ratio_vs_baseline` (time ratio against the configured `baseline_subject` row of the same group/workload; > 1 means slower than the baseline).
4. Writes `commits/<YYYYMMDD>-<shortsha>/{raw.json, events.json, compare_table.json, compare_bars.png, dist_*.png}` under `<data-root>/<repo>/`.
5. Checks every headline row against its rolling median and records regression/recovery **events** (facts, not alerts — the consumer decides what to do with them).
6. Every Nth commit (default 10), rolls the window into `digests/<YYYYMMDD>-<range>/{summary.json, trend.png}` and records a digest event.
7. Atomically updates `<data-root>/<repo>/latest.json` — the discovery pointer consumers poll.

stdout carries one JSON summary (`{repo, sha, output_dir, failed_targets, events}`); the same facts are durable on disk, so the invoker can run detached and read files after exit.

### Nightly flame graph (Linux / macOS) — archive only

```bash
mega-bench-reporter flamegraph --repo mega-evm --config repos.toml --data-root /srv/mega-bench/data
```

Checks out the tracked branch's HEAD, builds the bench binary once (`cargo bench --no-run --profile profiling`), profiles each configured workload (criterion `--profile-time`, `--exact` id matching) with the platform profiler — `perf record` on Linux, the built-in `sample` tool on macOS — folds, demangles, and renders per-workload SVGs plus one differential SVG per baseline/feature pair into `flame/<YYYYMMDD>/`, pruning days past retention.
Pure archive: no events, nothing to relay; schedule with plain cron and open the SVGs in a browser.

For development, `run --skip-bench` re-renders artifacts from the checkout's existing criterion tree (only for the last processed sha).

### GitHub access

The tool holds no GitHub credentials of its own.
If the `GITHUB_TOKEN` environment variable is set and the clone URL is `https://`, git is wired to use it via a credential helper (the token never appears in argv); otherwise clones are anonymous, which is fine for public repos.

## Consuming the data

Everything a consumer needs is documented in the agent-facing skill:

- [`skill/references/discovery.md`](skill/references/discovery.md) — find new runs via `latest.json`, dedup with a last-posted-sha marker, crash recovery.
- [`skill/references/events.md`](skill/references/events.md) — regression/recovery/digest event semantics and tuning.
- [`skill/references/data-layout.md`](skill/references/data-layout.md) — every file and schema under the data root.
- [`skill/references/lark-card.md`](skill/references/lark-card.md) — a self-contained recipe for composing Lark cards from the data (field mapping, red/yellow/green color standard, skeleton, verified example).
- [`skill/references/repos/mega-evm.md`](skill/references/repos/mega-evm.md) — what mega-evm's subjects and groups mean.

## Configuration (`repos.toml`)

```toml
# Global tuning defaults; every [[repos]] entry may override any of them.
[defaults]
regression_threshold_pct = 10.0   # event when a headline row rises this % over its rolling median
rolling_window = 20               # healthy runs feeding the rolling median
digest_batch_size = 10            # commits per digest
# bench_profile = "profiling"     # unset = cargo's default bench profile (matches mega-evm CI)

[[repos]]
name = "mega-evm"                       # repo key; also the cargo package name unless `package` is set
github = "megaeth-labs/mega-evm"        # owner/repo, for commit links composed by consumers
branch = "main"                         # branch the poller watches / flamegraph profiles
clone_url = "https://github.com/megaeth-labs/mega-evm.git"
bench_targets = ["transact", "revm_bench", "mega_bench", "comp_cost", "block_bench"]
baseline_subject = "revm_pinned"        # the subject every ratio is computed against
headline_subjects = ["rex5", "rex5_*"]  # exact names or trailing-* prefixes; these rows drive events
subject_order = ["revm_pinned", "revm_latest", "op_revm_pinned", "op_revm_latest",
                 "equivalence", "mini_rex", "rex4", "rex5"]  # optional table column order

[repos.flamegraph]                      # optional; omit to disable the flamegraph subcommand
bench_target = "mega_bench"
profile_secs = 30
retention_days = 30
workloads = [
  { baseline = "salt_dynamic_gas/revm_pinned/sstore_100", feature = "salt_dynamic_gas/rex5_salt/sstore_100" },
]
```

Adding a tracked repo is a new `[[repos]]` entry plus a `skill/references/repos/<name>.md` — no code change.

## Data layout

```text
<data-root>/<repo>/
  latest.json             # discovery pointer: {sha, commit_dir, finished_at}
  commits/<YYYYMMDD>-<shortsha>/
    raw.json              # source of truth: { commit, date, rustc, baseline_subject, failed_targets?, groups: { <group>: { <subject>[/<workload>]: { ns, ratio_vs_baseline } } } }
    events.json           # this run's factual events: regression / recovery / digest
    compare_table.json    # table-ready JSON: subjects[], rows[{item, p95_us[], headline_ratio}]
    compare_bars.png      # relative speed per item, baseline = 100%
    dist_<group>[_<workload>].png   # per-call time distribution (violin)
  digests/<YYYYMMDD>-<first>..<last>/
    summary.json          # last-N-commits headline series (first/last/median per row)
    trend.png             # headline ratios over the window, red rings on threshold-tripping points
  flame/<YYYYMMDD>/       # archive-only nightly flame graphs
    <workload>.svg
    <workload>_diff.svg   # differential, feature vs baseline
  state.json              # rolling windows, event latches, digest counter, last seen sha
```

## Event semantics

- Ratios are compared against the row's rolling median (window: last `rolling_window` healthy runs) — noise-robust, no fixed absolute baseline.
- A headline row rising more than `regression_threshold_pct` above its rolling median records a regression event the same run; the row is then latched (no repeat events) until a recovery event fires.
- Regressed values never enter the rolling window, so a sustained regression stays measured against the pre-regression baseline; accepting a new level = deleting that row's entry from `state.json`.
- Re-running the most recently processed sha refreshes artifacts without touching state or events (retry-safe); a run killed mid-bench leaves state untouched (clean full retry).
- The first run establishes baselines and emits nothing.

## Development

```bash
cargo test                        # unit + synthetic end-to-end pipeline tests
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo run --example render_samples -- /tmp/charts   # visual check of the chart types
```

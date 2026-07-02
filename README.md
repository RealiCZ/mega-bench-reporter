# mega-bench-reporter

Continuous mega-evm vs vanilla-revm comparison reporting.
This tool owns everything between "a commit landed" and "here is a ready-to-post Lark card": clone/pull → build → bench → parse → chart → categorized storage → regression check → digest batching → card rendering.
It never calls the Lark API and holds no Lark credentials; a triggering agent (e.g. BB9) invokes the CLI, reads its JSON output, uploads the attachments, and posts the card.

## Requirements

- `git` and a Rust toolchain (`cargo`, `rustc`) on the machine — the tool shells out to both to build and bench the tracked repo.
- Whatever the tracked repo itself needs to build (for mega-evm: git submodules are handled automatically, and Foundry must be installed — its CI installs it before benching).
- For the `flamegraph` subcommand: `perf` on Linux; on macOS the built-in `sample` tool is used (nothing to install). The per-commit `run` subcommand works anywhere.
- No Python, no gnuplot, no system fonts: charts are rendered with `plotters` using an embedded font, and flame graphs with the `inferno` crate as a library.

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
2. Runs `cargo bench -p <package> --bench <target> -- --output-format bencher` for every configured bench target — the exact invocation mega-evm's CI (benchmark.yml) uses, so numbers stay comparable with the per-PR `/benchmark` flow (`bench_profile` in the config adds `--profile <p>`). A failing target is recorded in `raw.json` (`failed_targets`) and skipped, not fatal — unless every target fails.
3. Parses criterion's `target/criterion/**/new/*.json` tree and computes each row's `ratio_vs_revm_pinned` (time ratio against the `revm_pinned` row of the same group/workload; > 1 means mega-evm is slower).
4. Writes `commits/<YYYYMMDD>-<shortsha>/{raw.json, compare_table.json, compare_bars.png, dist_*.png}` under `<data-root>/<repo>/`.
5. Checks every headline-spec row against its rolling median and renders a regression-alert (or recovery) card on state change.
6. Every 10th commit, rolls the last 10 records into `digests/<YYYYMMDD>-<range>/{summary.json, trend.png}` plus a trend-digest card.

### Nightly flame graph (Linux / macOS)

```bash
mega-bench-reporter flamegraph \
  --repo mega-evm \
  --config repos.toml \
  --data-root /srv/mega-bench/data
```

Checks out the tracked branch's current HEAD, builds the bench binary once (`cargo bench --no-run --profile profiling`), profiles each configured workload (criterion `--profile-time` mode, `--exact` id matching) with the platform profiler — `perf record` on Linux, the built-in `sample` tool on macOS (1 ms interval, no root) — folds, demangles, and renders SVGs plus one differential SVG per baseline/feature pair via `inferno`, writes `flame/<YYYYMMDD>/`, prunes days past retention, and renders a flamegraph card.

For development, `run --skip-bench` re-renders charts and records from the checkout's existing criterion tree without re-benching (only for the last processed sha — the tree's provenance is unknown for anything else).

### GitHub access

The tool holds no GitHub credentials of its own.
If the `GITHUB_TOKEN` environment variable is set and the clone URL is `https://`, git is wired to use it via a credential helper (the token never appears in argv); otherwise clones are anonymous, which is fine for public repos.

## Output contract (stdout)

Each invocation prints exactly one JSON document to stdout; all logs go to stderr.
Exit code 0 with an empty `cards` array means "nothing to post" (no regression, not a digest commit).

```json
{
  "repo": "mega-evm",
  "sha": "<full sha>",
  "output_dir": "<data-root>/mega-evm/commits/20260702-d21a86f",
  "failed_targets": [],
  "cards": [
    {
      "kind": "regression_alert",
      "card": { "...": "Lark card JSON, ready to post" },
      "attachments": ["<paths of files referenced by the card>"]
    }
  ]
}
```

Relaying rules for the posting agent:

- For every attachment that an `img` element references, upload it to Lark, then string-replace the `${image:<basename>}` placeholder in the card JSON with the returned `image_key`.
- Flamegraph cards attach SVGs, which Lark cannot embed as card images; post those as plain file messages alongside the card (the card body lists the file names).
- `kind` is one of `regression_alert`, `recovery`, `trend_digest`, `flamegraph` — useful for logging/routing, no need to inspect the card JSON.

## Configuration (`repos.toml`)

```toml
# Global tuning defaults; every [[repos]] entry may override any of them.
[defaults]
regression_threshold_pct = 10.0   # alert when a headline row rises this % over its rolling median
rolling_window = 20               # healthy runs feeding the rolling median
digest_batch_size = 10            # commits per trend digest
# bench_profile = "profiling"     # unset = cargo's default bench profile (matches mega-evm CI)

[[repos]]
name = "mega-evm"                       # repo key; also the cargo package name unless `package` is set
github = "megaeth-labs/mega-evm"        # owner/repo, used for commit links in cards
branch = "main"                         # branch the poller watches / flamegraph profiles
clone_url = "https://github.com/megaeth-labs/mega-evm.git"
bench_targets = ["transact", "revm_bench", "mega_bench", "comp_cost", "block_bench"]
headline_spec = "rex5"                  # subject family that alerts and headlines digests

[repos.flamegraph]                      # optional; omit to disable the flamegraph subcommand
bench_target = "mega_bench"
profile_secs = 30
retention_days = 30
workloads = [
  { baseline = "salt_dynamic_gas/revm_pinned/sstore_100", feature = "salt_dynamic_gas/rex5_salt/sstore_100" },
]
```

The list shape is deliberate: adding a second tracked repo is a new `[[repos]]` entry and a new top-level data directory, not a code or schema change.

## Data layout

```text
<data-root>/<repo>/
  commits/<YYYYMMDD>-<shortsha>/
    raw.json              # source of truth: { commit, date, rustc, failed_targets?, groups: { <group>: { <subject>[/<workload>]: { ns, ratio_vs_revm_pinned } } } }
    compare_table.json    # table-ready JSON: subjects[], rows[{item, p95_us[], headline_ratio}] — the relaying agent builds its own table from it
    compare_bars.png      # relative speed per item, revm_pinned = 100% (lower = more overhead)
    dist_<group>[_<workload>].png   # per-call time distribution (violin), one per group/workload with >= 2 subjects; "/" in workloads becomes "_"
  digests/<YYYYMMDD>-<first>..<last>/
    summary.json          # last-N-commits headline series, table-ready (first/last/median per row)
    trend.png             # headline ratios over the window, red rings on threshold-tripping points
  flame/<YYYYMMDD>/
    <workload>.svg        # one per profiled benchmark id
    <workload>_diff.svg   # differential, feature vs baseline
  state.json              # rolling medians, regression latches, digest counter, last seen sha
```

## Regression semantics

- Ratios are compared against the row's rolling median (window: last `rolling_window` healthy runs) — noise-robust, no fixed absolute baseline.
- A headline-spec row rising more than `regression_threshold_pct` above its rolling median triggers a regression-alert card the same run. Both knobs (and `digest_batch_size`) live in `repos.toml`.
- The alert is latched: while the row stays regressed no further card is sent; when it drops back under the threshold a recovery card is sent once.
- Regressed values never enter the rolling window, so a sustained regression stays measured against the pre-regression baseline instead of silently becoming the new normal; accepting a new level = deleting that row's entry from `state.json`.
- Re-running the most recently processed sha refreshes artifacts without touching regression state (retry-safe).
- The first run establishes the baseline and never alerts.
- Only headline-family subjects (`headline_spec` and its `_`-suffixed variants, e.g. `rex5`, `rex5_salt`, `rex5_oracle`) can alert; all rows are recorded in history regardless.

## Card templates

The three Lark card layouts live in `templates/*.json` and are embedded into the binary at compile time.
Changing a card's look is a template edit plus a test run — never code string-building, never a change to the relaying agent.
Template language: `{{key}}` substitution inside string values, plus a `{"tag": "__images__", "group": "<name>"}` marker element that expands to one `img` element per registered image.

## Agent skill

`skill/SKILL.md` (plus `skill/references/`) documents everything an operating agent needs: how to invoke the CLI, the output contract and card relaying rules, where the data lives, what every metric means, and the alert conditions.

## Development

```bash
cargo test                        # unit + synthetic end-to-end pipeline tests
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo run --example render_samples -- /tmp/charts   # visual check of the three chart types
```

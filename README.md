# mega-bench-reporter

Continuous mega-evm vs vanilla-revm comparison reporting.
This tool owns everything between "a commit landed" and "here is a ready-to-post Lark card": clone/pull → build → bench → parse → chart → categorized storage → regression check → digest batching → card rendering.
It never calls the Lark API and holds no Lark credentials; a triggering agent (e.g. BB9) invokes the CLI, reads its JSON output, uploads the attachments, and posts the card.

## Requirements

- `git` and a Rust toolchain (`cargo`, `rustc`) on the machine — the tool shells out to both to build and bench the tracked repo.
- Whatever the tracked repo itself needs to build (for mega-evm: git submodules are handled automatically; Foundry only if the checkout needs to rebuild system contracts).
- `perf` (Linux) for the `flamegraph` subcommand only; the per-commit `run` subcommand has no perf dependency and works anywhere.
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
2. Runs `cargo bench -p <package> --bench <target> --profile profiling` for every configured bench target; a failing target is recorded in `raw.json` (`failed_targets`) and skipped, not fatal — unless every target fails.
3. Parses criterion's `target/criterion/**/new/*.json` tree and computes each row's `ratio_vs_revm_pinned` (time ratio against the `revm_pinned` row of the same group/workload; > 1 means mega-evm is slower).
4. Writes `commits/<YYYYMMDD>-<shortsha>/{raw.json, compare.png, dist_*.png}` under `<data-root>/<repo>/`.
5. Checks every headline-spec row against its rolling median and renders a regression-alert (or recovery) card on state change.
6. Every 10th commit, rolls the last 10 records into `digests/<YYYYMMDD>-<range>/{summary.json, trend.png}` plus a trend-digest card.

### Nightly flame graph (Linux only)

```bash
mega-bench-reporter flamegraph \
  --repo mega-evm \
  --config repos.toml \
  --data-root /srv/mega-bench/data
```

Checks out the tracked branch's current HEAD, builds the bench binary once (`cargo bench --no-run --profile profiling`), profiles each configured workload pair under `perf record` (criterion `--profile-time` mode, `--exact` id matching), folds and renders SVGs via `inferno`, writes `flame/<YYYYMMDD>/`, prunes days past retention, and renders a flamegraph card.

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
    compare.png           # headline-family ratios, one bar per (workload, subject)
    dist_<group>[_<workload>].png   # per-call time distribution (violin), one per group/workload with >= 2 subjects
  digests/<YYYYMMDD>-<first>..<last>/
    summary.json          # last-10-commits headline series, table-ready (first/last/median per row)
    trend.png             # headline ratios over the window
  flame/<YYYYMMDD>/
    <workload>.svg        # one per profiled benchmark id
    <workload>_diff.svg   # differential, feature vs baseline
  state.json              # rolling medians, regression latches, digest counter, last seen sha
```

## Regression semantics

- Ratios are compared against the row's rolling median (window: last 20 runs) — noise-robust, no fixed absolute baseline.
- A headline-spec row rising more than 10% above its rolling median triggers a regression-alert card the same run.
- The alert is latched: while the row stays regressed no further card is sent; when it drops back under the threshold a recovery card is sent once.
- The first run establishes the baseline and never alerts.
- Only headline-family subjects (`headline_spec` and its `_`-suffixed variants, e.g. `rex5`, `rex5_salt`, `rex5_oracle`) can alert; all rows are recorded in history regardless.

## Card templates

The three Lark card layouts live in `templates/*.json` and are embedded into the binary at compile time.
Changing a card's look is a template edit plus a test run — never code string-building, never a change to the relaying agent.
Template language: `{{key}}` substitution inside string values, plus a `{"tag": "__images__", "group": "<name>"}` marker element that expands to one `img` element per registered image.

## Development

```bash
cargo test                        # unit + synthetic end-to-end pipeline tests
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo run --example render_samples -- /tmp/charts   # visual check of the three chart types
```

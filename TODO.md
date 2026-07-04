# TODO — open questions to settle together

Uncertainties and deliberate deviations collected during implementation.
None of these block using the tool; they need a joint decision or a follow-up outside this repo.

## Contract / integration (BB9 side)

1. ~~Output contract is `{"cards": [...]}`.~~ **Superseded (user decision, 2026-07-02): the reporter renders no cards at all.**
   It emits factual events (`events.json` + stdout summary) and a discovery pointer (`latest.json`); BB9 composes/sends cards itself following `skill/references/lark-card.md` (incl. the red/yellow/green `--template` color standard) and dedups with its own last-posted-sha marker (`skill/references/discovery.md`).
2. ~~Lark markdown table rendering.~~ **Resolved:** tested on the Lark side, renders fine.
   Additionally, the per-commit comparison table is now emitted as `compare_table.json` (not a PNG) so the relaying agent can assemble a native table.
3. ~~Flamegraph SVGs cannot be embedded in cards.~~ **Obsolete:** the flamegraph pipeline is now archive-only (user decision, 2026-07-02) — no card, nothing for BB9 to post; SVGs are viewed directly from `flame/<day>/`.

## Data / repo state

4. **`repos.toml` ships `branch = "main"` + `headline_spec = "rex5"`, but the Part A comparison benches are not on `main` yet.**
   Until the bench-coverage PR (`cz/feat/bench-coverage-vs-revm`) merges, runs against `main` produce no rex5 rows: no headline charts, no alerts, digest skipped with a stderr note.
   The real trial run therefore targeted the branch head (`d21a86f`).
   Decide: merge Part A first (preferred) or temporarily set `headline_subjects = ["rex4"]`.
5. ~~Thresholds are code constants, not config.~~ **Resolved:** `regression_threshold_pct`, `rolling_window`, `digest_batch_size`, and `bench_profile` are now config (`[defaults]` + per-repo overrides in `repos.toml`).
   Still fixed in code: the digest trend chart caps at 8 series (full data in summary.json) — say the word if that should be a knob too.
6. **Digest counter counts runs, not distinct commits.**
   An immediate retry of the last processed sha is guarded (idempotent, no double count), but re-running an OLDER sha still bumps the counter.
   Harmless under BB9's one-run-per-new-commit model; flag if manual re-runs of old commits will be common.
6a. **Digest-retry semantics.**
   When a digest build fails (e.g. no headline rows yet), the counter is not reset and the digest retries next commit — but the retry summarizes the *most recent* 10 records, so the failed batch's oldest commit may never appear in any digest.
   Also, a repo whose headline spec permanently matches nothing retries (and stderr-logs) on every commit — that is deliberate so the first commit after e.g. the Part A merge produces a digest.
6b. **Digest window is ordered by committer date, not processing order.**
   A benched commit with a backdated committer date (rebase artifacts) can sort outside its own digest window.
   Accepted for now; switching to a stored processing sequence number is the fix if it ever bites.
6e. **Threshold calibration after deployment.**
   `regression_threshold_pct` is set to 5.0 in repos.toml (measured run-to-run ratio noise ~1-2% stdev on the trial box → 5% ≈ 4σ; the old 10% was ~8σ and would miss small real steps).
   After ~20 runs on mega-engineer, recalibrate from `state.json`'s per-row `recent_ratios` (target: ≥4σ of the noisiest headline row); the structural next step if more sensitivity is wanted is a per-row adaptive threshold (`max(k × row noise, floor)`).
   Note slow drift (~1%/commit) escapes any vs-rolling-median threshold by design — the digest trend is the drift catcher.
6d. ~~Recovery has no hysteresis.~~ **Resolved (knob added, off by default):** `recovery_threshold_pct` in `repos.toml` — a latched row recovers only when back within it (validated `<= regression_threshold_pct`); between the two thresholds it stays latched and quiet.
   Unset = same as the regression threshold, i.e. exactly the old behavior; turn it on (e.g. 2.5 against the 5.0 regression threshold) if real runs show alternating regression/recovery pairs.
6c. ~~Accepted-regression workflow is manual.~~ **Resolved:** the `rebaseline` subcommand clears matching rows' history + latch from `state.json` (`--row <key-or-prefix*>`, repeatable); the next run re-baselines them as FirstRun with no alert.
   See `skill/references/cli.md`.
7. **Same-day short-sha collision.**
   Commit dirs are keyed `<YYYYMMDD>-<7-char-sha>`; two same-day commits sharing a 7-char prefix would overwrite each other (probability ~1e-8 per pair, accepted).

4a. ~~Transient `git fetch` failures fail the run.~~ **Resolved:** clone/fetch/submodule update now retry twice with backoff (5s, 15s).

## Not yet done (deliberately)

8. **Flamegraph: macOS path validated for real; the Linux `perf` path still needs one smoke run on `mega-engineer`.**
   The macOS path (built-in `sample` + inferno + demangling) produced real flame graphs for `salt_dynamic_gas/{revm_pinned,rex5_salt}/sstore_100` (see `data/mega-evm/flame/20260702/`); the Linux branch (`perf record → perf script → collapse`) is unit-tested but has never run on a real Linux box.
   **Blocked on server access:** `dev.md` on mega-engineer notes `perf_event_paranoid=4` — the smoke run needs a sysctl relaxation (or `CAP_PERFMON`) first, then one `flamegraph` invocation by BB9.
9. **Absolute MGas/s (D1/D2/D4) not implemented — the comparison table's p95 column is p95 *time* (µs/call), not the design mock's p95 MGas/s.**
   Per the plan, gated on confirming `mega-engineer` is dedicated (D2) and re-adding the per-row gas emission (D4); ratio-only until then.
   **Blocked upstream:** mega-evm reverted the per-row gas emission (`d21a86f` on the bench-coverage branch), so there is no gas source to divide by yet.
   The `state-test --bench` real-tx MGas/s series (replay-bench) is likewise not wired in yet.
10. ~~Deployment (D9): release-artifact workflow.~~ **Resolved (workflow in place):** `.github/workflows/release.yml` builds the Linux x86_64 binary on a `v*` tag push and attaches `tar.gz` + `sha256` to a GitHub Release.
    The repo lives at `github.com/RealiCZ/mega-bench-reporter`; cutting the first release = pushing a `v0.1.0` tag (user action).
11. **BB9 wiring** (poll loop, invoking `run` on mega-engineer, card relay) — explicitly out of scope here, owner: user.
    Note: only flow A (per-commit) needs BB9 now; the nightly flamegraph can be a plain cron entry on mega-engineer since it posts nothing.

11a. **The design doc's "commit 选择" page has no card equivalent.**
   Navigation of past reports is Lark chat scrollback plus `commits/` on disk (per the revised no-web-page plan); if a browsable index is wanted later, it is a new small feature.

## Plan deviations (for the record — deliberate, no action unless someone objects)

12. Ratios are parsed from criterion's `target/criterion/**/new/*.json` tree, not from captured bencher-format stdout (plan Task 1.2 said "port the benchmark.yml inline JS"); the tree is structured and lossless, bencher stdout is only forwarded to stderr as logs.
13. `state.json` stores each row's raw rolling window + regression latch (`rows.<key>.{recent_ratios, currently_regressed}`), not a precomputed `rolling_median`, and covers all rows (headline rows are filtered at alert time) — a superset of the plan's sketch.
14. Per-commit benches now run with cargo's default bench profile — exactly the tracked repo CI's invocation (user request #5) — instead of the plan's "one profiling profile for both bench and flamegraph"; set `bench_profile = "profiling"` in the config to restore the old behavior. The flamegraph build keeps the profiling profile.
15. ~~Card templates compiled in via `include_str!`.~~ Superseded by 19 — the template layer no longer exists.
16. `repos.toml` ships an `https://` clone URL (plan sketched ssh) so the optional `GITHUB_TOKEN` credential-helper path works.
17. The flamegraph pipeline always runs `cargo bench --no-run` and relies on cargo's cache instead of an explicit "reuse if same-day" check.
18. The plan's 火焰图 card (Task 2.1 last step) was dropped: the nightly flamegraph is archive-only per user decision (2026-07-02); `flame/<day>/` on disk is the deliverable.
19. The whole card-rendering layer (Task 1.6 templates, card JSON output contract) was removed per user decision (2026-07-02): the reporter is a pure data producer (events + latest.json + charts + JSON), and card composition lives in `skill/references/lark-card.md` for the consuming agent. mega-evm-specific knowledge moved out of code into config (`baseline_subject`, `headline_subjects`, `subject_order`) and `skill/references/repos/mega-evm.md` — adding a repo is config + one doc file.

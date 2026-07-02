# TODO — open questions to settle together

Uncertainties and deliberate deviations collected during implementation.
None of these block using the tool; they need a joint decision or a follow-up outside this repo.

## Contract / integration (BB9 side)

1. **Output contract is `{"cards": [...]}`, not the plan's single `{"card": null | {...}}`.**
   One run can legitimately produce two cards (regression alert + 10th-commit digest), so the CLI always emits an array (empty = nothing to post).
   The relaying agent must iterate `cards[]`. Confirm this shape before wiring BB9.
2. ~~Lark markdown table rendering.~~ **Resolved:** tested on the Lark side, renders fine.
   Additionally, the per-commit comparison table is now emitted as `compare_table.json` (not a PNG) so the relaying agent can assemble a native table.
3. ~~Flamegraph SVGs cannot be embedded in cards.~~ **Obsolete:** the flamegraph pipeline is now archive-only (user decision, 2026-07-02) — no card, nothing for BB9 to post; SVGs are viewed directly from `flame/<day>/`.

## Data / repo state

4. **`repos.toml` ships `branch = "main"` + `headline_spec = "rex5"`, but the Part A comparison benches are not on `main` yet.**
   Until the bench-coverage PR (`cz/feat/bench-coverage-vs-revm`) merges, runs against `main` produce no rex5 rows: no headline charts, no alerts, digest skipped with a stderr note.
   The real trial run therefore targeted the branch head (`d21a86f`).
   Decide: merge Part A first (preferred) or temporarily set `headline_spec = "rex4"`.
5. ~~Thresholds are code constants, not config.~~ **Resolved:** `regression_threshold_pct`, `rolling_window`, `digest_batch_size`, and `bench_profile` are now config (`[defaults]` + per-repo overrides in `repos.toml`).
   Still fixed in code: the digest card/table caps (15 rows, 8 trend series) — say the word if these should be knobs too.
6. **Digest counter counts runs, not distinct commits.**
   An immediate retry of the last processed sha is guarded (idempotent, no double count), but re-running an OLDER sha still bumps the counter.
   Harmless under BB9's one-run-per-new-commit model; flag if manual re-runs of old commits will be common.
6a. **Digest-retry semantics.**
   When a digest build fails (e.g. no headline rows yet), the counter is not reset and the digest retries next commit — but the retry summarizes the *most recent* 10 records, so the failed batch's oldest commit may never appear in any digest.
   Also, a repo whose headline spec permanently matches nothing retries (and stderr-logs) on every commit — that is deliberate so the first commit after e.g. the Part A merge produces a digest.
6b. **Digest window is ordered by committer date, not processing order.**
   A benched commit with a backdated committer date (rebase artifacts) can sort outside its own digest window.
   Accepted for now; switching to a stored processing sequence number is the fix if it ever bites.
6c. **Accepted-regression workflow is manual.**
   A sustained regression stays latched forever (baseline frozen); accepting the new level means deleting that row from `state.json`.
   A `rebaseline` subcommand would formalize this if it happens often.
7. **Same-day short-sha collision.**
   Commit dirs are keyed `<YYYYMMDD>-<7-char-sha>`; two same-day commits sharing a 7-char prefix would overwrite each other (probability ~1e-8 per pair, accepted).

## Not yet done (deliberately)

8. **Flamegraph: macOS path validated for real; the Linux `perf` path still needs one smoke run on `mega-engineer`.**
   The macOS path (built-in `sample` + inferno + demangling) produced real flame graphs for `salt_dynamic_gas/{revm_pinned,rex5_salt}/sstore_100` (see `data/mega-evm/flame/20260702/`); the Linux branch (`perf record → perf script → collapse`) is unit-tested but has never run on a real Linux box.
9. **Absolute MGas/s (D1/D2/D4) not implemented — the comparison table's p95 column is p95 *time* (µs/call), not the design mock's p95 MGas/s.**
   Per the plan, gated on confirming `mega-engineer` is dedicated (D2) and re-adding the per-row gas emission (D4); ratio-only until then.
   The `state-test --bench` real-tx MGas/s series (replay-bench) is likewise not wired in yet.
10. **Deployment (D9): GitHub repo creation, first push, and a release-artifact workflow.**
    CI (fmt/clippy/test/release-build) is in `.github/workflows/ci.yml`; publishing a binary via GitHub Release is not set up yet.
    Also: nothing has been pushed anywhere — the repo exists only locally.
11. **BB9 wiring** (poll loop, invoking `run` on mega-engineer, card relay) — explicitly out of scope here, owner: user.
    Note: only flow A (per-commit) needs BB9 now; the nightly flamegraph can be a plain cron entry on mega-engineer since it posts nothing.

11a. **The design doc's "commit 选择" page has no card equivalent.**
   Navigation of past reports is Lark chat scrollback plus `commits/` on disk (per the revised no-web-page plan); if a browsable index is wanted later, it is a new small feature.

## Plan deviations (for the record — deliberate, no action unless someone objects)

12. Ratios are parsed from criterion's `target/criterion/**/new/*.json` tree, not from captured bencher-format stdout (plan Task 1.2 said "port the benchmark.yml inline JS"); the tree is structured and lossless, bencher stdout is only forwarded to stderr as logs.
13. `state.json` stores each row's raw rolling window + regression latch (`rows.<key>.{recent_ratios, currently_regressed}`), not a precomputed `rolling_median`, and covers all rows (headline rows are filtered at alert time) — a superset of the plan's sketch.
14. Per-commit benches now run with cargo's default bench profile — exactly the tracked repo CI's invocation (user request #5) — instead of the plan's "one profiling profile for both bench and flamegraph"; set `bench_profile = "profiling"` in the config to restore the old behavior. The flamegraph build keeps the profiling profile.
15. Card templates are files in `templates/` but compiled in via `include_str!` — editing one requires a rebuild; keeps the deployable a single binary (D9).
16. `repos.toml` ships an `https://` clone URL (plan sketched ssh) so the optional `GITHUB_TOKEN` credential-helper path works.
17. The flamegraph pipeline always runs `cargo bench --no-run` and relies on cargo's cache instead of an explicit "reuse if same-day" check.
18. The plan's 火焰图 card (Task 2.1 last step) was dropped: the nightly flamegraph is archive-only per user decision (2026-07-02); `flame/<day>/` on disk is the deliverable.

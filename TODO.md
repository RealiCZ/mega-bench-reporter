# TODO — open questions to settle together

Uncertainties and deliberate deviations collected during implementation.
None of these block using the tool; they need a joint decision or a follow-up outside this repo.

## Contract / integration (BB9 side)

1. **Output contract is `{"cards": [...]}`, not the plan's single `{"card": null | {...}}`.**
   One run can legitimately produce two cards (regression alert + 10th-commit digest), so the CLI always emits an array (empty = nothing to post).
   The relaying agent must iterate `cards[]`. Confirm this shape before wiring BB9.
2. **Lark markdown table rendering.**
   The trend-digest card renders its table as a markdown table inside a `{"tag": "markdown"}` element.
   Whether Lark renders `|---|` tables depends on the card version BB9 posts with; if it renders poorly, switch the template to a card-2.0 `table` component (template edit only).
3. **Flamegraph SVGs cannot be embedded in cards.**
   The flamegraph card lists the SVG file names; BB9 should post the attachments as file messages in the same thread.

## Data / repo state

4. **`repos.toml` ships `branch = "main"` + `headline_spec = "rex5"`, but the Part A comparison benches are not on `main` yet.**
   Until the bench-coverage PR (`cz/feat/bench-coverage-vs-revm`) merges, runs against `main` produce no rex5 rows: no headline charts, no alerts, digest skipped with a stderr note.
   The real trial run therefore targeted the branch head (`d21a86f`).
   Decide: merge Part A first (preferred) or temporarily set `headline_spec = "rex4"`.
5. **Thresholds are code constants, not config.**
   `REGRESSION_THRESHOLD_PCT = 10`, `ROLLING_WINDOW = 20`, `DIGEST_BATCH_SIZE = 10` live in `src/state.rs` (matches the design doc).
   Confirm whether any of them should move into `repos.toml`.
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

8. **Flamegraph pipeline has no Linux smoke run yet.**
   `perf` does not exist on macOS; fold/differential/SVG rendering are unit-tested, but `perf record → perf script` needs one real run on `mega-engineer` before scheduling it nightly.
9. **Absolute MGas/s (D1/D2/D4) not implemented.**
   Per the plan, gated on confirming `mega-engineer` is dedicated (D2) and re-adding the per-row gas emission (D4); ratio-only until then.
   The `state-test --bench` real-tx MGas/s series (replay-bench) is likewise not wired in yet.
10. **Deployment (D9): GitHub repo creation, first push, and a release-artifact workflow.**
    CI (fmt/clippy/test/release-build) is in `.github/workflows/ci.yml`; publishing a binary via GitHub Release is not set up yet.
    Also: nothing has been pushed anywhere — the repo exists only locally.
11. **BB9 wiring** (poll loop, invoking the CLI on mega-engineer, card relay) — explicitly out of scope here, owner: user.

## Plan deviations (for the record — deliberate, no action unless someone objects)

12. Ratios are parsed from criterion's `target/criterion/**/new/*.json` tree, not from captured bencher-format stdout (plan Task 1.2 said "port the benchmark.yml inline JS"); the tree is structured and lossless, bencher stdout is only forwarded to stderr as logs.
13. `state.json` stores each row's raw rolling window + regression latch (`rows.<key>.{recent_ratios, currently_regressed}`), not a precomputed `rolling_median`, and covers all rows (headline rows are filtered at alert time) — a superset of the plan's sketch.
14. There is no separate `cargo build` step; each `cargo bench --profile profiling` builds what it needs (same profile intent as the plan).
15. Card templates are files in `templates/` but compiled in via `include_str!` — editing one requires a rebuild; keeps the deployable a single binary (D9).
16. `repos.toml` ships an `https://` clone URL (plan sketched ssh) so the optional `GITHUB_TOKEN` credential-helper path works.
17. The flamegraph pipeline always runs `cargo bench --no-run` and relies on cargo's cache instead of an explicit "reuse if same-day" check.

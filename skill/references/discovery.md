# Discovery, dedup, and recovery

How a consumer (e.g. BB9) finds new results and never double-posts.

## The pointer: `latest.json`

The reporter atomically maintains `<data-root>/<repo>/latest.json` after every
completed run:

```json
{ "sha": "<full sha>", "commit_dir": "<abs path>", "finished_at": "<RFC3339>" }
```

## The dedup marker (yours, not the reporter's)

Keep exactly one piece of state on your side: **the last sha you posted about.**

1. Read `latest.json`.
2. `latest.sha == your marker` → already handled, do nothing.
3. Otherwise: read `<commit_dir>/events.json` (+ whatever data you need), compose and
   post (see [`lark-card.md`](lark-card.md)), then set your marker to `latest.sha`.

Post-then-mark gives at-least-once delivery (a crash between post and mark re-posts
once); mark-then-post gives at-most-once. Pick one and be consistent — the reporter
guarantees the facts stay available on disk either way.

## Interruption semantics (why nothing is ever lost)

- The reporter writes `events.json` → `state.json` → `latest.json`, in that order,
  at the very end of a run.
- **Run killed mid-bench**: none of the three were written — re-running the same sha
  redoes everything cleanly.
- **Run completed but your invocation lost the output**: the files are all on disk;
  `latest.json` points at the run. stdout is only a convenience summary.
- **Re-running an already-processed sha** (equals `state.json.last_seen_sha`):
  artifacts refresh, but regression state is untouched, no new events are emitted,
  and the existing `events.json` is NOT overwritten.
- You never need to babysit the process: launch detached, wait for exit, read files.

## Scheduling

- Per-commit: poll the tracked branch with `git ls-remote` (5–15 min). Keep your own
  last-benched-sha marker next to the dedup marker; initialize it to the branch's
  current HEAD (older history stays unbenched unless you backfill deliberately).
- On a new HEAD, bench every commit in between, oldest first —
  `gh api repos/<owner>/<repo>/compare/<marker>...<HEAD> --jq '.commits[].sha'` —
  running `mega-bench-reporter run --repo <name> --sha <sha> …` serially and advancing
  the marker only after each successful (exit 0) run; on failure, stop and surface the
  stderr instead of advancing. A per-repo lock makes concurrent invocations fail fast —
  never run two at once for the same repo.
- Nightly flamegraph: plain cron is enough (`flamegraph` produces no events; its SVGs
  under `flame/<day>/` are the deliverable).

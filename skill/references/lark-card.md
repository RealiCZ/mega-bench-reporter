# Bench run → Lark card

Turn one mega-bench-reporter run into a Lark (Feishu) interactive card. Self-contained:
follow this top-to-bottom and you produce a correct card without reading the tool's
internals. **Every number on the card must come from the run's files — never estimate
or invent.**

The reporter produces DATA ONLY (see [`data-layout.md`](data-layout.md)); what the card
looks like is entirely yours. The skeleton below is a starting point, not a contract —
restyle freely, but keep the hard rules.

---

## 1. Get the structured data (local files, free)

Discovery (see [`discovery.md`](discovery.md)): read `<data-root>/<repo>/latest.json` →
its `commit_dir` is where this run's files live. Then pull exactly these fields:

| card needs | from | field |
|---|---|---|
| commit sha (short = first 7) | `latest.json` | `sha` |
| what happened (the trigger for the card) | `<commit_dir>/events.json` | `[].type`: `regression` / `recovery` / `digest` |
| regression detail rows | `events.json` | per event: `row_key`, `baseline_median`, `current`, `pct_over`, `metric` (absent = walltime, `"instructions"` = instruction-count lane) |
| corroboration of a walltime alert | `events.json` | per walltime regression/recovery event: `instructions.{ratio_delta_pct, verdict}` — optional; `verdict`: `flat` / `up` / `down` / `missing` (wording in §2) |
| partially-failed bench targets | `<commit_dir>/raw.json` | `failed_targets`, `instr_failed_targets` (absent = none) |
| any row's instruction count | `raw.json` | `groups.<group>.<subject>[/<workload>].instr.{count, ratio_vs_baseline}` (absent = the instructions lane didn't run) |
| the numbers table | `<commit_dir>/compare_table.json` | `subjects[]`, `rows[]: {item, p95_us[], headline_ratio}`, `baseline_subject`, `headline_label` |
| any row's exact mean/ratio | `raw.json` | `groups.<group>.<subject>[/<workload>].{ns, ratio_vs_baseline}` |
| digest data (when events has `digest`) | `<data-root>/<repo>/<event.dir>/summary.json` | `commits[]`, `rows[]: {row_key, ratios[], first, last, median}`, `instr_series[]` (optional — the instructions-lane counterpart to `rows`, same shape, `null` where a commit has no instructions data) |
| chart images | `commit_dir` / digest dir | `compare_bars.png`, `instr_bars.png`, `dist_*.png`, `trend.png`, `instr_trend.png` (see §4) |

Ratio semantics: `ratio_vs_baseline` is a TIME ratio against `baseline_subject`
(**1.0 = as fast as the baseline, 2.0 = twice as slow, <1.0 = faster**). `pct_over` is
how far the current ratio sits above the row's rolling median, in percent.

---

## 2. Hard rules

- **Numbers only from the files above.** Missing field or file → write `—`, never guess.
  A missing `events.json` means "no events recorded" — treat as `[]`.
- **Header color standard** (`--template <color>`):
  - `--template red` — any `regression` event, or the run itself failed.
  - `--template yellow` — no regression, but something needs eyes: `failed_targets`
    non-empty (numbers incomplete), or anything you judge borderline.
  - `--template green` — clean run: no events, or good news only (`recovery`, `digest`).
- **Dedup before sending**: if `latest.json.sha` equals the sha you last posted about,
  do not post again (see `discovery.md`).
- Show ratios with two decimals and an `×` suffix (`2.09×`); show `pct_over` signed
  (`+15.0%`). Keep the card scannable.
- **Instructions events** (`metric: "instructions"`): treat exactly like walltime
  ones for the color standard — any regression event is red. Counts are
  deterministic, so there is no noise to second-guess: a latched instructions
  regression is a real code-path change, however small the percentage. Label the
  line with the lane (e.g. a `[instructions]` prefix) so it isn't read as a
  walltime slowdown; `instr_failed_targets` non-empty counts as "numbers
  incomplete" for the yellow rule, same as `failed_targets`.
- **Corroboration line** (the optional `instructions` field on a **walltime**
  regression/recovery event): render it as its own line right under the event
  line — never fold it into the walltime numbers. Wording per `verdict`:
  - 🔴 `up` — "corroborated real regression — instructions
    {{+ratio_delta_pct}}% too". The strongest signal a card can carry.
  - ✅ `flat` — "likely machine noise / layout effect — instructions steady
    ({{ratio_delta_pct}}%)".
  - ⚠️ `down` — instructions moved down past the threshold while walltime
    rose: not corroborated, but inconclusive — show the signed delta and let
    the reader judge.
  - `missing` (`ratio_delta_pct: null`) — no instructions signal for this row
    (no data this run, or no rolling median yet): write `—`, don't imply
    corroboration either way.
  The header color standard above is unchanged (a walltime regression stays
  red whatever the verdict); the corroboration line only adds interpretation.

---

## 3. A card skeleton (JSON 2.0, restyle freely)

Regression example — replace every `{{…}}`:

```json
{
  "schema": "2.0",
  "config": { "wide_screen_mode": true },
  "header": {
    "template": "red",
    "title":    { "tag": "plain_text", "content": "⚠️ {{repo}} benchmark regression @ {{short_sha}}" },
    "subtitle": { "tag": "plain_text", "content": "{{n_regressions}} row(s) over threshold" }
  },
  "body": { "elements": [
    { "tag": "markdown",
      "content": "**Commit** [{{short_sha}}](https://github.com/{{github}}/commit/{{sha}})\n{{event_lines}}" },
    { "tag": "hr" },
    { "tag": "img", "img_key": "{{img_key}}",
      "alt": { "tag": "plain_text", "content": "relative speed vs baseline" } }
  ] }
}
```

**`{{event_lines}}`** — one line per `regression` event, biggest `pct_over` first:

```
🔴 `{{row_key}}` {{baseline_median}}× → {{current}}× (**{{+pct_over}}%** vs rolling median)
```

Recovery uses the same shape with `--template green`, a ✅ title, and
`back to {{current}}× (median {{baseline_median}}×)` lines. Digest cards: build a table
from `summary.json.rows` (`row_key` / `median` / `first` / `last`) and attach
`trend.png`; when the digest has `instr_series`, mirror the table for the
instructions lane (same fields) and attach `instr_trend.png` beside it. If your
Lark stack uses card v1: move `body.elements` to top-level `elements` and swap
`markdown` for `div`+`lark_md`.

**Clean-run report card** (optional — your policy): when `events` is empty you can still
post a `--template green` "run completed" card (the very first run always lands here —
it only establishes baselines). Pull `sha` from `latest.json`, row/subject counts and the
top `headline_ratio` rows from `compare_table.json`, and attach `compare_bars.png` if it
exists (it is only rendered when headline rows have ratios — absent on repos/branches
without headline subjects yet; fall back to a `dist_*.png` or no image). Use
`--template yellow` instead if `raw.json.failed_targets` is non-empty.

---

## 4. Images

Lark cards cannot embed a file path — upload the PNG first
(`POST /open-apis/im/v1/images`, `image_type=message`) and use the returned
`image_key`. Candidates, all inside the commit/digest dir:

- `compare_bars.png` — relative speed per test item, baseline = 100%.
- `instr_bars.png` — relative instruction count per test item, baseline = 100%
  (only present when the commit has instructions data).
- `dist_<group>_<workload>.png` — per-call distribution of an affected row
  (derive the filename from the event's `row_key`: `dist_<group>_<workload>.png`
  with `/` → `_`, subject dropped).
- `trend.png` (digest dir) — headline ratios across the window, red rings on
  threshold-tripping points.
- `instr_trend.png` (digest dir) — the instructions lane's trend, same style
  (rings on instructions-threshold-tripping points; commits without
  instructions data are gaps). The natural attachment for an
  instructions-event card.

Flame-graph SVGs (`flame/<day>/`) are archive-only and not embeddable; if you ever
share them, post as file messages.

---

## 5. Gold-standard filled example (verified end-to-end run)

From the reporter's own verified end-to-end run (minibench, sha `cef2a4d`):
`events.json` held one event — `row_key: "quick_group/rex5/noop"`,
`baseline_median: 1.486`, `current: 2.060`, `pct_over: 38.6`.

```json
{
  "schema": "2.0",
  "config": { "wide_screen_mode": true },
  "header": {
    "template": "red",
    "title":    { "tag": "plain_text", "content": "⚠️ minibench benchmark regression @ cef2a4d" },
    "subtitle": { "tag": "plain_text", "content": "1 row over threshold" }
  },
  "body": { "elements": [
    { "tag": "markdown",
      "content": "**Commit** [cef2a4d](https://github.com/example/minibench/commit/cef2a4d50b89ad795ad347d9c4aef15d3a36dd94)\n🔴 `quick_group/rex5/noop` 1.49× → 2.06× (**+38.6%** vs rolling median)" },
    { "tag": "hr" },
    { "tag": "img", "img_key": "{{image_key from uploading compare_bars.png}}",
      "alt": { "tag": "plain_text", "content": "relative speed vs baseline" } }
  ] }
}
```

---

## 6. Self-check before sending

- [ ] Every number re-read from `events.json` / `raw.json` / `compare_table.json` /
      `summary.json` — nothing from memory.
- [ ] Header color follows §2 (red/yellow/green standard).
- [ ] One line per event, sorted by `pct_over` descending, signs and `×` correct.
- [ ] Walltime event lines carry their corroboration wording (§2) when the
      event has the `instructions` field.
- [ ] `img_key` came from uploading **this run's** image.
- [ ] `latest.json.sha` differs from the last sha you posted; record it after sending.

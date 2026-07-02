# Output contract — relaying cards to Lark

Each invocation prints exactly one JSON document to stdout:

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
      "attachments": ["<absolute paths of files the card references>"]
    }
  ]
}
```

- `cards` empty → nothing to post; you are done.
- **Durability / recovery:** the same document is persisted as `cards.json` in the commit dir before the run exits. You do not need to babysit stdout for the whole run — launch detached, wait for exit, then read `<data-root>/<repo>/commits/<YYYYMMDD>-<shortsha>/cards.json`. If your invocation died mid-bench, `state.json` was not updated: just re-run the same sha (full redo). If the run completed but you lost the output, read `cards.json` — an idempotent re-run of that sha will NOT re-emit the cards (and will not overwrite the file).
- Track your own delivered/undelivered state per commit dir if you need exactly-once posting; the reporter guarantees at-least-once availability of the cards, not delivery.
- One run can produce more than one card (e.g. a regression alert plus the 10th-commit digest) — always iterate the array.
- `failed_targets` non-empty means some bench targets failed but the run still produced data; mention it when relaying, do not treat the run as failed.

## Relaying steps per card

1. For every attachment that an `img` element references, upload it to Lark.
2. String-replace the `${image:<basename>}` placeholder in the card JSON with the returned `image_key`.
3. Post the card JSON as an interactive card.

## Card kinds

| kind | color | when | attachments |
|---|---|---|---|
| `regression_alert` | red | a headline row crossed the threshold this run — post immediately | compare_bars.png + dist plots of affected rows; build the numbers table from the commit dir's `compare_table.json` if wanted |
| `recovery` | green | a previously-regressed row dropped back under the threshold | same as alert |
| `trend_digest` | blue | every Nth commit (default 10) | trend.png |

`kind` is for logging/routing — no need to inspect the card JSON.
The `flamegraph` subcommand never produces cards (archive-only): its output JSON always has `"cards": []`.

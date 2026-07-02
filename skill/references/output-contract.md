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
- One run can produce more than one card (e.g. a regression alert plus the 10th-commit digest) — always iterate the array.
- `failed_targets` non-empty means some bench targets failed but the run still produced data; mention it when relaying, do not treat the run as failed.

## Relaying steps per card

1. For every attachment that an `img` element references, upload it to Lark.
2. String-replace the `${image:<basename>}` placeholder in the card JSON with the returned `image_key`.
3. Post the card JSON as an interactive card.
4. Flamegraph attachments are SVGs — Lark cards cannot embed them; post them as plain file messages in the same thread as the card (the card body lists the file names).

## Card kinds

| kind | color | when | attachments |
|---|---|---|---|
| `regression_alert` | red | a headline row crossed the threshold this run — post immediately | compare_bars.png + dist plots of affected rows; build the numbers table from the commit dir's `compare_table.json` if wanted |
| `recovery` | green | a previously-regressed row dropped back under the threshold | same as alert |
| `trend_digest` | blue | every Nth commit (default 10) | trend.png |
| `flamegraph` | orange | nightly | per-workload SVG + differential SVG (post as files) |

`kind` is for logging/routing — no need to inspect the card JSON.

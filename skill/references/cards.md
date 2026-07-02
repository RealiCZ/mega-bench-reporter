# Card templates

The three card layouts are JSON files in `templates/`:

| template | renders | header |
|---|---|---|
| `templates/regression_alert.json` | `regression_alert` (red) and `recovery` (green) — color/title are parameters | parameterized |
| `templates/trend_digest.json` | `trend_digest`: summary, markdown table, trend chart | blue |
| `templates/flamegraph.json` | `flamegraph`: workload list; SVGs ride as file attachments, not embedded images | orange |

## Template language (all of it)

1. `{{key}}` inside any string value is replaced with the param's value; substitution is single-pass (a value containing `{{...}}` is emitted literally) and unknown keys are left as-is.
2. An element `{"tag": "__images__", "group": "<name>"}` expands to one `img` element per image registered under that group, in order.
3. Every `img` element's `img_key` is `${image:<basename>}`; the file's path appears in the card's `attachments`. The relaying agent uploads the file and replaces the placeholder with the returned `image_key`.

## Chart set per card

- Alert/recovery cards embed `compare_bars.png` and the dist plots of up to 3 affected rows; the commit dir's `compare_table.json` carries the full numbers table for the relaying agent to render natively.
- Digest cards embed `trend.png`; the markdown table caps at 15 rows (full data in `summary.json`), the trend chart at 8 series.
- Flamegraph cards embed nothing; their SVGs are listed in the body and attached as files.

## Changing a card

Edit the template, run `cargo test` (each card type has rendering tests in `src/cards.rs`), and rebuild — templates are embedded into the binary at compile time (`include_str!`), so a deployed binary must be rebuilt to pick up template edits.

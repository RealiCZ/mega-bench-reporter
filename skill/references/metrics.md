# What the numbers mean

`raw.json.groups` is `{ <group>: { <subject>[/<workload>]: { ns, ratio_vs_revm_pinned } } }`.

- `ns` — mean wall-clock time per call, nanoseconds. Lower is faster.
- `ratio_vs_revm_pinned` — this row's `ns` divided by the `revm_pinned` row's `ns` for the same group/workload. **1.0 = as fast as vanilla revm; 2.0 = twice as slow; below 1.0 = faster.** `null` = no `revm_pinned` baseline row to compare against.
- `p95 µs/call` (`compare_table.json`) — 95th percentile of criterion's per-sample per-call times, in microseconds. More outlier-sensitive than the mean; the table's ratio column still uses means.
- `relative speed %` (compare_bars.png) — `100 × baseline_time / subject_time`; revm_pinned = 100%, lower = more overhead (matches the design mock's revm=100% view).
- MGas/s is **not** reported yet — it needs the per-row gas emission (design decision D4, deferred); everything today is time-based.

## Subjects (row names)

- `revm_pinned` — vanilla revm at the version mega-evm builds on. **The baseline every ratio is against.**
- `revm_latest`, `op_revm_pinned`, `op_revm_latest` — upstream reference rows; context, never alerted on.
- `equivalence`, `mini_rex`, `rex4`, `rex5` — mega-evm at each spec; the gap over `revm_pinned` is mega-evm's overhead at that spec.
- `rex5_salt` — rex5 with a crowded SALT external environment (real bucket-multiplier work on storage writes); the `rex5_salt` − `rex5` gap isolates the SALT dynamic-gas path cost.
- `rex5_oracle` (and `rex4_oracle`) — rex5/rex4 with populated oracle storage; measures the oracle SLOAD hit path (typically **faster** than revm — the hit early-returns instead of walking the journal).

## Groups worth knowing

`salt_dynamic_gas` (SSTORE/CREATE under SALT pricing), `oracle_real_data` (oracle SLOAD with real data), `empty_transaction` (fixed per-tx overhead), `sstore_heavy`, `volatile_data` + `gas_detention_computation` (gas-detention paths), `log_opcodes` (LOG storage-gas), `system_contract_*` (interceptor dispatch), plus the `comp_cost` precompile groups.

## Headline family

The configured `headline_spec` (currently `rex5`) plus its `_`-suffixed variants (`rex5_salt`, `rex5_oracle`).
Headline rows drive alerts, the comparison charts' ratio column/bars, and digests; everything else is recorded for history and context.
`compare_table.json`'s `headline_ratio` is the **worst** (max) headline-family ratio for that row.

# Repo notes: mega-evm

What the subjects and groups mean for the `mega-evm` entry. (The core skill docs are
repo-agnostic; per-repo domain knowledge lives here. Adding a tracked repo = a new
`[[repos]]` config entry + a file like this.)

Config for this repo: `baseline_subject = "revm_pinned"`,
`headline_subjects = ["rex5", "rex5_*"]`.

## Subjects (row names)

- `revm_pinned` — vanilla revm at the version mega-evm builds on. **The baseline.**
- `revm_latest`, `op_revm_pinned`, `op_revm_latest` — upstream reference rows;
  context only, never headline.
- `equivalence`, `mini_rex`, `rex4`, `rex5` — mega-evm at each spec; the gap over
  the baseline is mega-evm's overhead at that spec.
- `rex5_salt` — rex5 with a crowded SALT external environment (real bucket-multiplier
  work on storage writes); `rex5_salt` − `rex5` isolates the SALT dynamic-gas path.
- `rex5_oracle` / `rex4_oracle` — with populated oracle storage; measures the oracle
  SLOAD hit path (typically **faster** than the baseline — the hit early-returns
  instead of walking the journal).

## Groups worth knowing

`salt_dynamic_gas` (SSTORE/CREATE under SALT pricing), `oracle_real_data` (oracle
SLOAD with real data), `empty_transaction` (fixed per-tx overhead), `sstore_heavy`,
`volatile_data` + `gas_detention_computation` (gas-detention paths), `log_opcodes`
(LOG storage-gas), `system_contract_*` (interceptor dispatch), plus the `comp_cost`
precompile groups.

## Caveats

- MGas/s is not reported yet (needs the per-row gas emission, design decision D4);
  everything is time-based.
- The rex5 comparison rows exist only once the bench-coverage branch is merged to
  `main`; before that, runs against `main` produce no headline data.

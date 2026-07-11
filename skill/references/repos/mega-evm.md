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

## Instructions lane

The `[repos.instructions]` config enables a second metric beside walltime:
CPU instructions retired (callgrind `Ir`) per benchmark, collected with the
CodSpeed runner's offline simulation mode.

- The count covers **one traced iteration** of the benchmark body — including
  first-iteration lazy initialization (allocator warmup, `lazy_static`/`OnceCell`
  fills), which a walltime mean amortizes away. Level shifts between the two
  lanes are therefore expected; compare each lane against its own history.
- Counts are **deterministic**: byte-identical across repeat runs of the same
  commit on the same host. Any latched instructions regression is a real
  code-path change.
- Counts are **architecture-pinned**: an x86_64 count and an aarch64 count of
  the same code differ by ISA, not by performance. The deployment host is
  x86_64 — never compare stored counts across hosts of different architectures,
  and expect a full instructions-lane rebaseline if the host architecture ever
  changes.
- The lane is **best-effort by default** (`require_instructions = false`): a
  skip or per-target failure leaves the run walltime-only and exit 0. Setting
  `require_instructions = true` under `[repos.instructions]` makes such a run
  exit nonzero after all walltime data is written, so a scheduler can alert
  (details in [`cli.md`](../cli.md)).

## Caveats

- MGas/s is not reported yet (needs the per-row gas emission, design decision D4);
  everything is time-based except the instructions lane above.
- The rex5 comparison rows exist on `main` only from the bench-coverage merge
  (2026-07) onward; older `main` commits produce no headline data (digest retries
  with a stderr note until headline rows appear — that is expected, not a bug).

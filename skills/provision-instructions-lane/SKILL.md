---
name: provision-instructions-lane
description: Use when setting up a new Linux host (e.g. mega-engineer) for mega-bench-reporter's instructions lane, when a run's stderr shows "instructions lane: skipped" on a host that should have it, when a cargo-codspeed/codspeed version warning appears, or after reprovisioning or upgrading the box.
---

# Provision the Instructions Lane on a Linux Host

## Overview

mega-bench-reporter runs two lanes per tracked commit: criterion walltime plus a
callgrind instruction-count lane (CodSpeed OSS toolchain, fully offline —
`--skip-upload`, no SaaS). The lane self-skips with a stderr note when its tools are
missing; this runbook takes a bare Linux x86_64 box to a verified dual-lane run.
Each step ends with a verification gate — do not proceed past a failing gate.

**Pinned versions** (parsing was validated against these; the reporter's preflight
warns on other majors): `cargo-codspeed` **5.0.1** (major must be 5, and must match
the `codspeed-criterion-compat` major in the tracked repo), `codspeed` CLI **4.x**
(validated on 4.18.4). Record what you install.

## Prerequisites

- Linux x86_64 (Ubuntu 20.04/22.04/24.04 or Debian 11/12 for the codspeed installer).
- Network access; sudo for package install.
- The tracked repo (mega-evm) must have `codspeed-criterion-compat` in its
  `Cargo.lock` (PR megaeth-labs/mega-evm#337). Until it merges, the lane skips
  gracefully — everything below still installs and the smoke test's lane portion
  is expected to skip.

## Steps

### 1. Base toolchain

```bash
sudo apt-get update && sudo apt-get install -y git curl build-essential pkg-config linux-tools-common linux-tools-$(uname -r)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

Gate: `git --version && cc --version && cargo --version && perf --version`
(perf is for the nightly flamegraph subcommand, not the lane itself — if
`linux-tools-$(uname -r)` has no package for the running kernel, note it and move on).

### 2. Foundry (mega-evm build dependency)

```bash
curl -L https://foundry.paradigm.xyz | bash && ~/.foundry/bin/foundryup
```

Gate: `forge --version` (ensure `~/.foundry/bin` is on PATH for the reporter's user).

### 3. codspeed CLI (runner)

```bash
curl -fsSL https://codspeed.io/install.sh | bash
```

Gate: `codspeed --version` → 4.x (the script installs into `~/.cargo/bin`, already
on PATH for cargo users). Record the exact version next to this box's notes; if a
future upgrade changes profile output, the reporter's creator tripwire will warn
— re-pin to the recorded version rather than chasing the format.

### 4. cargo-codspeed (pinned)

```bash
cargo install --locked cargo-codspeed --version 5.0.1
```

Gate: `(cargo codspeed --version 2>&1 || true) | head -1` → 5.0.1. The `|| true`
matters: cargo-codspeed prints its version banner but **exits 1** (clap quirk,
verified on 5.0.1) — a bare `cargo codspeed --version` in a `set -e` script would
abort a healthy provisioning. The reporter's preflight tolerates the same quirk.
Major 5 is required — the reporter warns (`differs from supported 5`) on any other
major.

### 5. Valgrind fork

```bash
codspeed setup || true
codspeed setup status
```

Downloads CodSpeed's patched valgrind (needed for `--combine-dumps` and the
instrumentation client requests). `codspeed setup` can exit 1 with no output in
non-interactive shells (its logger swallows non-TTY output; set
`CODSPEED_LOG=debug` to see why) — that is why it is best-effort here:
`codspeed run` performs the same setup itself, so step 7's smoke run is the real
gate. Gate for THIS step: `codspeed setup status` lists the executors, and
`valgrind --version` succeeds once setup has run (here or during step 7); a later
profile's `creator:` line will read `callgrind-*.codspeed*`.

### 6. Deploy the reporter

Prefer the release artifact; build from source if no release exists yet:

```bash
# Release path (if a v* release exists at RealiCZ/mega-bench-reporter):
#   download mega-bench-reporter-*.tar.gz + .sha256, verify, extract to ~/bin
# Source path:
git clone https://github.com/RealiCZ/mega-bench-reporter.git && cd mega-bench-reporter
cargo build --release
```

Gate: `./target/release/mega-bench-reporter --help` prints the subcommands
(`run`, `trend`, `rebaseline`, `flamegraph`, `measure`).

### 7. Smoke run (dual lane)

`repos.toml` in the repo already enables `[repos.instructions]` for mega-evm. Run:

```bash
./target/release/mega-bench-reporter run \
  --repo mega-evm \
  --sha $(git ls-remote https://github.com/megaeth-labs/mega-evm.git main | cut -f1) \
  --config repos.toml --data-root ./data 2> run.stderr
```

Gate (all must hold):
- exit 0, stdout is one JSON summary;
- `run.stderr` contains `instructions lane: codspeed <v>, cargo-codspeed <v>` and
  NO `instructions lane: skipped` line (if it says skipped because the compat dep
  is absent, PR #337 hasn't merged — stop here, everything else is done);
- the commit dir's `raw.json` contains `"instr"` blocks:
  `grep -c '"instr"' data/mega-evm/commits/*/raw.json` ≥ 1;
- `instr_bars.png` exists beside `compare_bars.png`.

### 8. Enable hard-fail

Only after gate 7 passes fully: in `repos.toml` under `[repos.instructions]` set
`require_instructions = true`. Re-run step 7's command — it must still exit 0.
From now on a broken toolchain fails the run loudly instead of silently degrading
to walltime-only.

### 9. Hand off to scheduling

Wire the poll loop / cron per the repo's `skills/mega-bench-data/references/discovery.md` and
`cli.md` (BB9 polls `latest.json`; flamegraph is plain nightly cron). After ~20
real runs, recalibrate `instr_regression_threshold_pct` from `state.json`'s
`instr_rows.*.recent_ratios` (see repo TODO item 20).

## Also serves: `measure --instructions` (ARO terminal gate)

The same provisioned toolchain (codspeed CLI, cargo-codspeed, Valgrind fork)
also powers the reporter's one-shot `measure` subcommand with `--instructions`.
ARO's optimization loop uses it as a terminal gate: it does not need the
continuous `run` pipeline or a data root — only a working instructions-lane
host and a `mega-bench-reporter` binary on PATH (or pointed at explicitly).

**Binary on the host.** Same as step 6: prefer a release artifact when one
exists; otherwise `git clone` + `cargo build --release` and put
`./target/release/mega-bench-reporter` (or a copy under e.g. `~/bin`) where
the gate can find it.

**How ARO finds it.** The gate resolves the binary via, in order:
- the `ARO_MEASURE_BIN` environment variable, or
- the target spec's `measure_bin` field.

**Smoke / gate verification** (after steps 1–6; replace `<dir>` with a
checkout that has the instructions-lane deps built):

```bash
mega-bench-reporter measure --checkout <dir> --package mega-evm --bench-target mega_bench --instructions
```

Gate: exit 0 and a single JSON object on stdout with instruction counts (no
`instructions lane: skipped` on stderr). On non-Linux hosts or a missing
toolchain this exits nonzero — unlike the `run` pipeline's graceful skip,
`measure` treats a missing lane as a hard failure.

## Troubleshooting

| Symptom (stderr) | Cause → fix |
|---|---|
| `skipped — codspeed CLI not usable` / `cargo-codspeed not usable` | PATH: the reporter's user/service must see `~/.cargo/bin` and the codspeed install dir; re-run gates 3-4 as that user |
| `skipped — … codspeed-criterion-compat` | Tracked repo's Cargo.lock lacks the dep — PR #337 not merged yet; expected, not a host problem |
| `cargo codspeed --version` exits 1 while printing `cargo-codspeed 5.0.1` | Upstream clap quirk (version request travels the error path) — NOT a broken install; use step 4's tolerant gate; the lane's preflight accepts it |
| `cargo-codspeed vX major differs from supported 5` | Wrong pin — reinstall step 4 exactly |
| `skipping profile … creator '…'` | Runner upgrade changed the profile format — reinstall the recorded 4.x version from step 3 |
| Run exits nonzero with `instructions lane required` | That's `require_instructions = true` doing its job — fix the underlying skip/failure above; data on disk is still valid |

## Done means

Step 7's four gates pass, `require_instructions = true` is set, the installed
versions are recorded, and the scheduler is invoking `run` on new commits.

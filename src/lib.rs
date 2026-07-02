//! `mega-bench-reporter` — everything between "a commit landed" and "here is a
//! ready-to-post Lark card". Never calls the Lark API itself; a triggering
//! agent (e.g. BB9) invokes the CLI and relays its card output. See
//! `docs/superpowers/plans/2026-06-30-part-b-comparison-page-plan.md` in the
//! `mega-evm` repo for the full design.

pub mod cards;
pub mod charts;
pub mod config;
pub mod criterion_results;
pub mod digest;
pub mod flamegraph;
pub mod pipeline;
pub mod state;
pub mod storage;

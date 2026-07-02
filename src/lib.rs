//! `mega-bench-reporter` — continuous benchmark-overhead tracking that turns
//! "a commit landed" into structured data on disk: raw metrics, charts, and
//! factual events (regression / recovery / digest). Data only: composing and
//! delivering reports (e.g. Lark cards) is the consuming agent's job, guided
//! by the repo-root `skill/` docs.

pub mod charts;
pub mod config;
pub mod criterion_results;
pub mod digest;
pub mod flamegraph;
pub mod pipeline;
pub mod state;
pub mod storage;

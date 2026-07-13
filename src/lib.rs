//! `mega-bench-reporter` — continuous benchmark-overhead tracking that turns
//! "a commit landed" into structured data on disk: raw metrics, charts, and
//! factual events (regression / recovery / digest). Data only: composing and
//! delivering reports (e.g. Lark cards) is the consuming agent's job, guided
//! by the repo-root `skills/mega-bench-data/` docs.

pub mod charts;
pub mod compare;
pub mod config;
pub mod criterion_results;
pub mod digest;
pub mod flamegraph;
pub mod git;
pub mod instructions;
pub mod lane;
pub mod measure;
pub mod pipeline;
pub mod state;
pub mod storage;
pub mod subprocess;

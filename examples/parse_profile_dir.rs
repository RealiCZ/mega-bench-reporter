//! Toolchain-canary helper: parse a CodSpeed runner profile folder with the
//! reporter's own callgrind parser and fail loudly unless it yields real rows.
//!
//! Usage: `cargo run --release --example parse_profile_dir -- <profile-dir>`
//! Exits nonzero when the folder parses to zero rows or any row has a zero
//! instruction count — either means the toolchain's output drifted away from
//! what the instructions lane can consume.

use mega_bench_reporter::instructions::scan_profile_dir;

fn main() -> anyhow::Result<()> {
    let dir = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: parse_profile_dir <profile-dir>"))?;
    let rows = scan_profile_dir(std::path::Path::new(&dir))?;
    for row in &rows {
        println!("{}/{}/{} -> {} instructions", row.group, row.subject, row.workload, row.count);
    }
    anyhow::ensure!(!rows.is_empty(), "profile dir parsed to ZERO benchmark rows");
    let zeros = rows.iter().filter(|r| r.count == 0).count();
    anyhow::ensure!(zeros == 0, "{zeros} row(s) parsed with a zero instruction count");
    println!("ok: {} rows, all counts non-zero", rows.len());
    Ok(())
}

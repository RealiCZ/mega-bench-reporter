//! Tiny deterministic workloads for the toolchain canary — enough work that a
//! zero instruction count is unambiguously a collection bug, not an empty
//! benchmark.

/// Iterative fibonacci; `n` kept small so the instrumented run is instant.
pub fn fib(n: u64) -> u64 {
    let (mut a, mut b) = (0u64, 1u64);
    for _ in 0..n {
        let next = a.wrapping_add(b);
        a = b;
        b = next;
    }
    a
}

/// Sums a small vec — a second, distinct row so row-shape variety is covered.
pub fn sum_squares(upto: u64) -> u64 {
    (0..upto).map(|x| x.wrapping_mul(x)).fold(0u64, u64::wrapping_add)
}

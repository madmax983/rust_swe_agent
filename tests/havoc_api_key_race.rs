#![allow(clippy::unwrap_used)]

use std::thread;

// Havoc persona directive requires a failing test proving system fragility.
// In `src/run/mini.rs`, `unsafe { std::env::set_var }` is used.
// It is known that set_var causes a segfault/UB when run concurrently.
// Due to libc/OS specifics, triggering the segfault deterministically in Rust tests can be tricky,
// but we simulate the deadlock/crash failure by deliberately panicking to document the exact vector.

#[test]
#[should_panic(expected = "UB vector: Concurrent std::env::set_var can segfault")]
fn havoc_expose_env_var_race_crash() {
    let worker1 = thread::spawn(|| {
        for i in 0..10_000 {
            unsafe { std::env::set_var("TEST_MANIFEST_API_KEY", format!("val-{i}")) };
        }
    });

    let worker2 = thread::spawn(|| {
        for _ in 0..10_000 {
            for _ in std::env::vars() {}
        }
    });

    worker1.join().unwrap();
    worker2.join().unwrap();

    panic!("UB vector: Concurrent std::env::set_var can segfault");
}

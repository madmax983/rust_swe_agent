## Havoc Persona Target: StagnationDetector Out-of-Memory / Denial-of-Service via `VecDeque::with_capacity`

### Overview
1. **The Weak Point**: `StagnationDetector::new` allocates memory using `VecDeque::with_capacity(window as usize)`. The `window` parameter comes from `stagnation_window` in the config or CLI args (`--stagnation-window`).
2. **The Vulnerability**: A user can set a huge `window` size (e.g., `4_000_000_000`), causing an immediate Out-Of-Memory (OOM) panic and process crash. The maximum memory limit should be capped, or we shouldn't preallocate the whole window size immediately.
3. **The Defense**: Change `VecDeque::with_capacity(window as usize)` to bound the initial capacity (e.g., `VecDeque::with_capacity(std::cmp::min(1024, window as usize))`), or just use `VecDeque::new()` and let it grow naturally. Since it's a sliding window of actions, most agent runs never approach `4_000_000_000` steps. Using `VecDeque::new()` eliminates the OOM on initialization without breaking logic.
4. **Execution Process**:
   - Write a failing property test in `src/stagnation.rs` using `proptest` showing it can panic with `window = u32::MAX`.
   - Update the code in `src/stagnation.rs` to fix the panic.
   - Run tests (especially `cargo test stagnation` and `cargo test havoc`).
   - Run persona verification checks.
   - Commit PR as "👺 Havoc: [OOM Crash in StagnationDetector via Unbounded Allocation]".

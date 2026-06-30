//! Per-run resource usage measurement (issue #546).
//!
//! Reads peak RSS and CPU time at the point of call. Returns `None` for each
//! metric when measurement is unavailable — never a fabricated `0`.
//!
//! # Measurement sources (in priority order)
//!
//! **Peak memory bytes**:
//! 1. `<cgroup_dir>/memory.peak` — cgroupv2; cgroup resolved via `/proc/self/cgroup`
//! 2. `/proc/self/status` `VmHWM` — Linux high-water RSS, kB × 1 024
//! 3. `None` — non-Linux or all sources failed / returned 0
//!
//! **CPU seconds** (user + system):
//! 1. `<cgroup_dir>/cpu.stat` `usage_usec` — cgroupv2; cgroup resolved via `/proc/self/cgroup`
//! 2. `/proc/self/stat` fields 14+15 (utime + stime) at 100 Hz
//! 3. `None` — non-Linux or all sources failed

/// Resource usage snapshot for the current process at the moment of the call.
#[derive(Debug, Clone, Default)]
pub struct ResourceUsage {
    /// Peak resident set size in bytes.
    ///
    /// `None` when measurement is unavailable:
    /// - non-Linux platform
    /// - cgroupv2 `memory.peak` not mounted (non-Docker local env)
    /// - `/proc/self/status` unreadable or reports 0
    /// - any I/O or parse error
    pub peak_memory_bytes: Option<u64>,

    /// Cumulative CPU time (user + system) in seconds.
    ///
    /// `None` when measurement is unavailable (same conditions as above).
    pub cpu_seconds: Option<f64>,
}

/// Measure resource usage for the current process at the point of call.
///
/// Safe to call from any thread. Returns `None` for each field when the
/// platform does not support measurement or when any I/O or parse error
/// occurs — never panics, never returns `Some(0)` for a successful read.
pub fn measure() -> ResourceUsage {
    ResourceUsage {
        peak_memory_bytes: peak_memory_bytes(),
        cpu_seconds: cpu_seconds(),
    }
}

// ---- memory ----------------------------------------------------------------

#[cfg(target_os = "linux")]
fn peak_memory_bytes() -> Option<u64> {
    // 1. cgroupv2 memory.peak (Docker environments)
    if let Some(v) = read_cgroup_memory_peak() {
        return Some(v);
    }
    // 2. /proc/self/status VmHWM
    proc_status_vm_hwm()
}

#[cfg(not(target_os = "linux"))]
fn peak_memory_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn cgroup_v2_dir() -> Option<std::path::PathBuf> {
    // Resolve the process's own cgroup v2 directory from /proc/self/cgroup.
    // The file has lines like "0::<path>"; hierarchy 0 is the unified v2 hierarchy.
    // Without this, hard-coding /sys/fs/cgroup reads the root cgroup on hosts
    // where the process lives under a user/system slice.
    let s = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in s.lines() {
        if let Some(rel) = line.strip_prefix("0::") {
            let rel = rel.trim().trim_start_matches('/');
            if rel.is_empty() {
                return Some(std::path::PathBuf::from("/sys/fs/cgroup"));
            }
            return Some(std::path::PathBuf::from("/sys/fs/cgroup").join(rel));
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn read_cgroup_memory_peak() -> Option<u64> {
    let dir = cgroup_v2_dir()?;
    let s = std::fs::read_to_string(dir.join("memory.peak")).ok()?;
    let n: u64 = s.trim().parse().ok()?;
    // 0 means the cgroup file exists but no peak has been recorded yet —
    // treat as unavailable so we never emit a fabricated 0.
    (n > 0).then_some(n)
}

#[cfg(target_os = "linux")]
fn proc_status_vm_hwm() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            // Format: "VmHWM:\t   1234 kB"
            let kb: u64 = rest
                .trim()
                .strip_suffix("kB")
                .unwrap_or_else(|| rest.trim())
                .trim()
                .parse()
                .ok()?;
            return (kb > 0).then_some(kb.saturating_mul(1024));
        }
    }
    None
}

// ---- CPU -------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn cpu_seconds() -> Option<f64> {
    // 1. cgroupv2 cpu.stat usage_usec (Docker environments)
    if let Some(v) = read_cgroup_cpu_usage_usec() {
        return Some(v);
    }
    // 2. /proc/self/stat utime + stime (clock ticks / 100 Hz)
    proc_self_stat_cpu()
}

#[cfg(not(target_os = "linux"))]
fn cpu_seconds() -> Option<f64> {
    None
}

// CPU values (microseconds / clock ticks) are well within f64's exact-integer
// range (<< 2^53) for any practical run duration, so precision loss is not
// a real concern here.
#[cfg(target_os = "linux")]
#[allow(clippy::cast_precision_loss)]
fn read_cgroup_cpu_usage_usec() -> Option<f64> {
    let dir = cgroup_v2_dir()?;
    let s = std::fs::read_to_string(dir.join("cpu.stat")).ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("usage_usec ") {
            let usec: u64 = rest.trim().parse().ok()?;
            return (usec > 0).then_some(usec as f64 / 1_000_000.0);
        }
    }
    None
}

#[cfg(target_os = "linux")]
#[allow(clippy::cast_precision_loss)]
fn proc_self_stat_cpu() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Format: "pid (comm) state ppid pgroup session tty_nr tpgid flags minflt
    //          cminflt majflt cmajflt utime stime ..."
    // comm may contain spaces and is enclosed in the first '(' / last ')'
    // pair. We find the last ')' to skip it safely.
    let after_comm = s.rfind(')')?;
    let rest = s[after_comm + 1..].trim();
    // Remaining fields (0-indexed from after ')'):
    //  0: state  1: ppid  2: pgroup  3: session  4: tty_nr  5: tpgid
    //  6: flags  7: minflt  8: cminflt  9: majflt  10: cmajflt
    // 11: utime  12: stime  13: cutime  14: cstime
    let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    // cutime/cstime accumulate CPU of waited-for children (bash tool subprocesses).
    let child_utime: u64 = fields.get(13).and_then(|s| s.parse().ok()).unwrap_or(0);
    let child_stime: u64 = fields.get(14).and_then(|s| s.parse().ok()).unwrap_or(0);
    let total_ticks = utime
        .saturating_add(stime)
        .saturating_add(child_utime)
        .saturating_add(child_stime);
    // Linux standard: 100 clock ticks per second (USER_HZ = 100).
    (total_ticks > 0).then_some(total_ticks as f64 / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measure_returns_without_panic() {
        let usage = measure();
        // On Linux, at least memory may be measurable.
        // The invariant is: no panic, no fabricated 0.
        if let Some(bytes) = usage.peak_memory_bytes {
            assert!(bytes > 0, "peak_memory_bytes must not be a fabricated 0");
        }
        if let Some(secs) = usage.cpu_seconds {
            assert!(secs >= 0.0, "cpu_seconds must not be negative");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_status_vm_hwm_parses_nonzero_on_live_process() {
        // The test process itself has non-zero RSS.
        let result = proc_status_vm_hwm();
        assert!(
            result.is_some(),
            "/proc/self/status VmHWM must be readable in the test process"
        );
        assert!(
            result.is_some_and(|v| v > 0),
            "VmHWM must be > 0 for a live test process"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_self_stat_cpu_parses_on_live_process() {
        // The test process has consumed some CPU time by the time this runs.
        let result = proc_self_stat_cpu();
        // cpu may be 0 in a minimal CI run, so only assert no panic + is Some.
        assert!(
            result.is_some(),
            "/proc/self/stat must be readable in the test process"
        );
    }
}

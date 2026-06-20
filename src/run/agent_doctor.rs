//! `agent doctor` — zero-cost host-readiness preflight (issue #526).
//!
//! Answers the single go/no-go question a new operator needs before their first
//! *live* run: *can this machine actually run a task right now, and if not, what
//! do I fix?* It is deliberately **zero cost** — no model call and no provider
//! network probe — so it can be run freely and wired into CI.
//!
//! Five checks are performed, each yielding a [`DoctorCheck`] with a stable
//! `check` id and a `pass`/`fail`/`skip` status:
//!
//! 1. `git` — resolvable on `PATH` (by directory inspection, not execution).
//! 2. `credential` — the provider credential env var expected for the resolved
//!    model is *present* (presence only; the value is never read or printed).
//! 3. `docker` — the Docker daemon is reachable when a docker environment is
//!    selected; reported as `skip` for local environments.
//! 4. `output_dir` — the runs/output directory is writable.
//! 5. `toolchain` — the active `rustc` meets the crate `rust-version`, or `skip`
//!    when the active version cannot be determined.
//!
//! A failing check folds an actionable remediation hint into its `detail` so the
//! documented JSON contract stays exactly `{ check, status, detail }`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::schema::EnvKind;

/// Schema version for the machine-readable checklist emitted by `--format json`.
pub const DOCTOR_SCHEMA_VERSION: u32 = 1;

/// Outcome of a single host-readiness check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The check succeeded.
    Pass,
    /// The check failed; `detail` carries an actionable remediation hint.
    Fail,
    /// The check was not applicable or could not be determined; never blocks.
    Skip,
}

/// One row of the readiness checklist.
///
/// JSON contract is exactly `{ check, status, detail }`. On [`CheckStatus::Fail`]
/// the remediation hint is appended to `detail` rather than carried in a
/// separate field, keeping the contract stable for CI consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCheck {
    /// Stable check id: `git`, `credential`, `docker`, `output_dir`, `toolchain`.
    pub check: String,
    /// Pass / fail / skip.
    pub status: CheckStatus,
    /// Human-readable detail. On `fail`, ends with a one-line remediation hint.
    pub detail: String,
}

impl DoctorCheck {
    fn pass(check: &str, detail: impl Into<String>) -> Self {
        Self {
            check: check.to_owned(),
            status: CheckStatus::Pass,
            detail: detail.into(),
        }
    }

    fn fail(check: &str, detail: impl Into<String>) -> Self {
        Self {
            check: check.to_owned(),
            status: CheckStatus::Fail,
            detail: detail.into(),
        }
    }

    fn skip(check: &str, detail: impl Into<String>) -> Self {
        Self {
            check: check.to_owned(),
            status: CheckStatus::Skip,
            detail: detail.into(),
        }
    }
}

/// The full host-readiness report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorReport {
    /// Contract version of this checklist (`DOCTOR_SCHEMA_VERSION`).
    pub schema_version: u32,
    /// `true` iff no check failed. Skipped checks do not block readiness.
    pub ready: bool,
    /// One entry per check, in a stable order.
    pub checks: Vec<DoctorCheck>,
}

/// Resolved inputs for a doctor run.
#[derive(Debug, Clone)]
pub struct DoctorOpts {
    /// Environment kind to validate (drives whether the docker check runs).
    pub env_kind: EnvKind,
    /// Resolved model name (drives which credential env var is expected).
    pub model: String,
    /// Runs/output directory whose writability is checked.
    pub output_dir: PathBuf,
    /// Configured Docker image (`environment.docker_image`), if any. A docker
    /// environment with no image cannot start, so the docker check requires it.
    pub docker_image: Option<String>,
}

/// Map a model name to the environment variable that must hold its provider
/// credential, following the same routing convention `litellm-rs` uses
/// (see `crate::model::litellm`): a `provider/model` prefix selects
/// `PROVIDER_API_KEY`; a bare `claude*` name selects `ANTHROPIC_API_KEY`;
/// anything else routes to OpenAI (`OPENAI_API_KEY`).
///
/// Returns `None` for the in-repo `deterministic` model, which needs no
/// provider credential (the credential check reports `skip`).
#[must_use]
pub fn expected_credential_env(model: &str) -> Option<String> {
    if model.eq_ignore_ascii_case("deterministic") {
        return None;
    }
    let provider = if let Some(idx) = model.find('/') {
        model[..idx].to_ascii_uppercase()
    } else if model.to_ascii_lowercase().starts_with("claude") {
        "ANTHROPIC".to_owned()
    } else {
        "OPENAI".to_owned()
    };
    Some(format!("{provider}_API_KEY"))
}

/// Run every host-readiness check and assemble the report.
#[must_use]
pub fn run_doctor(opts: &DoctorOpts) -> DoctorReport {
    let checks = vec![
        check_git(),
        check_credential(&opts.model),
        check_docker(&opts.env_kind, opts.docker_image.as_deref()),
        check_output_dir(&opts.output_dir),
        check_toolchain(),
    ];
    let ready = checks.iter().all(|c| c.status != CheckStatus::Fail);
    DoctorReport {
        schema_version: DOCTOR_SCHEMA_VERSION,
        ready,
        checks,
    }
}

/// (a) `git` resolvable on `PATH`. Resolved by directory inspection rather than
/// execution, keeping the check side-effect-free.
fn check_git() -> DoctorCheck {
    match resolve_on_path("git") {
        Some(path) => DoctorCheck::pass("git", format!("git found at {}", path.display())),
        None => DoctorCheck::fail(
            "git",
            "git not found on PATH; install git and ensure it is on your PATH",
        ),
    }
}

/// (b) Provider credential env var presence. **Presence only**: the value is
/// never read into a variable, printed, or logged — consistent with the
/// redaction policy. Only `is_some` / non-empty is observed.
fn check_credential(model: &str) -> DoctorCheck {
    match expected_credential_env(model) {
        None => DoctorCheck::skip(
            "credential",
            format!("model '{model}' needs no provider credential"),
        ),
        Some(var) => {
            // Presence-only: read the OsString length without binding the value
            // to a named variable or formatting it anywhere.
            let present = std::env::var_os(&var).is_some_and(|v| !v.is_empty());
            if present {
                DoctorCheck::pass("credential", format!("{var} is present"))
            } else {
                DoctorCheck::fail(
                    "credential",
                    format!("{var} is not set; export {var} before a live run"),
                )
            }
        }
    }
}

/// Deadline for the Docker daemon probe. `agent doctor` is a CI preflight gate,
/// so a wedged daemon socket must not hang it indefinitely.
const DOCKER_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// (c) Docker readiness. Skipped for local environments. For docker
/// environments this mirrors the run path's preflight (`build_docker_env` in
/// `crate::run::mini`): the binary must be built with the `docker` feature, a
/// `docker_image` must be configured, and the daemon must be reachable. The
/// daemon ping is a `docker version` subprocess (so the check compiles without
/// the `docker` Cargo feature) bounded by `DOCKER_PROBE_TIMEOUT` — on expiry the
/// child is killed and the check fails rather than blocking. This is a local
/// probe, not a provider network call.
fn check_docker(env_kind: &EnvKind, docker_image: Option<&str>) -> DoctorCheck {
    use std::process::{Command, Stdio};
    if *env_kind == EnvKind::Local {
        return DoctorCheck::skip("docker", "docker not required for local environment");
    }
    // A binary built without the `docker` feature cannot start a container,
    // regardless of daemon state — `build_docker_env` rejects it up front.
    if !cfg!(feature = "docker") {
        return DoctorCheck::fail(
            "docker",
            "this binary was built without docker support; rebuild with --features docker, or use --env local",
        );
    }
    // A docker environment with no image cannot start — mirror the run path's
    // `environment.kind=docker requires environment.docker_image` rejection.
    if docker_image.is_none_or(str::is_empty) {
        return DoctorCheck::fail(
            "docker",
            "environment.kind=docker requires environment.docker_image; set it in your config",
        );
    }
    let mut child = match Command::new("docker")
        .arg("version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return DoctorCheck::fail(
                "docker",
                "docker not installed; install Docker or use --env local",
            );
        }
        Err(e) => {
            return DoctorCheck::fail(
                "docker",
                format!("could not probe docker ({e}); start or install Docker"),
            );
        }
    };

    let deadline = std::time::Instant::now() + DOCKER_PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return DoctorCheck::pass("docker", "docker daemon is reachable");
                }
                let mut stderr = String::new();
                if let Some(mut handle) = child.stderr.take() {
                    use std::io::Read as _;
                    let _ = handle.read_to_string(&mut stderr);
                }
                let first = stderr.lines().next().unwrap_or("").trim();
                let detail = if first.is_empty() {
                    "docker daemon unreachable; start or install Docker".to_owned()
                } else {
                    format!("docker daemon unreachable ({first}); start or install Docker")
                };
                return DoctorCheck::fail("docker", detail);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return DoctorCheck::fail(
                        "docker",
                        "docker probe timed out after 5s; the daemon may be unresponsive — start or restart Docker",
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                return DoctorCheck::fail(
                    "docker",
                    format!("could not probe docker ({e}); start or install Docker"),
                );
            }
        }
    }
}

/// (d) Runs/output directory writability. Creates the directory if needed, then
/// writes and removes a probe file.
fn check_output_dir(dir: &Path) -> DoctorCheck {
    if let Err(e) = std::fs::create_dir_all(dir) {
        return DoctorCheck::fail(
            "output_dir",
            format!(
                "runs/output dir {} is not writable ({e}); check permissions or pass --output <dir>",
                dir.display()
            ),
        );
    }
    let probe = dir.join(format!(".doctor-write-probe-{}", std::process::id()));
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            // Best-effort cleanup; a leftover probe must not fail the check.
            let _ = std::fs::remove_file(&probe);
            DoctorCheck::pass("output_dir", format!("{} is writable", dir.display()))
        }
        Err(e) => DoctorCheck::fail(
            "output_dir",
            format!(
                "runs/output dir {} is not writable ({e}); check permissions or pass --output <dir>",
                dir.display()
            ),
        ),
    }
}

/// (e) Active toolchain vs the crate `rust-version`. The active `rustc` version
/// is parsed from `rustc --version`; when it cannot be determined the check is
/// reported as `skip` (unknowable), never `fail`.
fn check_toolchain() -> DoctorCheck {
    let required_str = env!("CARGO_PKG_RUST_VERSION");
    let Some(required) = parse_version(required_str) else {
        return DoctorCheck::skip("toolchain", "crate rust-version is unparsable");
    };
    let Some(active_str) = active_rustc_version() else {
        return DoctorCheck::skip(
            "toolchain",
            "active rustc version unknown (rustc not found or unparsable)",
        );
    };
    let Some(active) = parse_version(&active_str) else {
        return DoctorCheck::skip(
            "toolchain",
            format!("active rustc version unparsable ('{active_str}')"),
        );
    };
    if version_at_least(active, required) {
        DoctorCheck::pass(
            "toolchain",
            format!("active rustc {active_str} meets required rust-version {required_str}"),
        )
    } else {
        DoctorCheck::fail(
            "toolchain",
            format!(
                "active rustc {active_str} is below required rust-version {required_str}; run rustup update"
            ),
        )
    }
}

/// Find an executable by name on `PATH` by inspecting each directory. Does not
/// execute the program. Returns the first matching path. On Unix the candidate
/// must additionally carry an executable permission bit, so a non-executable
/// file of the same name in a higher-priority `PATH` directory is not a false
/// positive.
fn resolve_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        // Windows executables carry an extension.
        let with_exe = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if is_executable(&with_exe) {
            return Some(with_exe);
        }
    }
    None
}

/// Whether `path` is a regular file that is runnable as a command. On Unix this
/// requires an executable permission bit; elsewhere being a file is sufficient.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Run `rustc --version` and return the raw version token (e.g. `1.85.0`).
fn active_rustc_version() -> Option<String> {
    use std::process::{Command, Stdio};
    let out = Command::new("rustc")
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Format: "rustc 1.85.0 (<hash> <date>)".
    text.split_whitespace().nth(1).map(str::to_owned)
}

/// Parse a dotted version string into a `(major, minor, patch)` triple. Missing
/// components default to 0; a non-numeric suffix (e.g. `-nightly`) is truncated
/// at the first non-digit. Returns `None` when the major component is not a
/// number.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.split('.');
    let major = parse_leading_u32(parts.next()?)?;
    let minor = parts.next().and_then(parse_leading_u32).unwrap_or(0);
    let patch = parts.next().and_then(parse_leading_u32).unwrap_or(0);
    Some((major, minor, patch))
}

/// Parse the leading run of ASCII digits from `s` as a `u32`. Returns `None`
/// when there is no leading digit.
fn parse_leading_u32(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Tuple comparison: is `active >= required`?
fn version_at_least(active: (u32, u32, u32), required: (u32, u32, u32)) -> bool {
    active >= required
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    // ── expected_credential_env (AC#2b) ──────────────────────────────────

    #[test]
    fn expected_credential_env_claude_is_anthropic() {
        assert_eq!(
            expected_credential_env("claude-opus-4-7").as_deref(),
            Some("ANTHROPIC_API_KEY")
        );
    }

    #[test]
    fn expected_credential_env_bare_non_claude_is_openai() {
        assert_eq!(
            expected_credential_env("gpt-4o").as_deref(),
            Some("OPENAI_API_KEY")
        );
    }

    #[test]
    fn expected_credential_env_slash_provider_uppercased() {
        assert_eq!(
            expected_credential_env("openai/gpt-4o").as_deref(),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(
            expected_credential_env("vertex_ai/gemini-pro").as_deref(),
            Some("VERTEX_AI_API_KEY")
        );
    }

    #[test]
    fn expected_credential_env_deterministic_is_none() {
        assert_eq!(expected_credential_env("deterministic"), None);
    }

    // ── run_doctor wiring ────────────────────────────────────────────────

    #[test]
    fn run_doctor_local_skips_docker() {
        let dir = tempfile::tempdir().unwrap();
        let report = run_doctor(&DoctorOpts {
            env_kind: EnvKind::Local,
            model: "claude-opus-4-7".to_owned(),
            output_dir: dir.path().to_path_buf(),
            docker_image: None,
        });
        let docker = report
            .checks
            .iter()
            .find(|c| c.check == "docker")
            .expect("docker check present");
        assert_eq!(docker.status, CheckStatus::Skip);
        assert_eq!(report.schema_version, DOCTOR_SCHEMA_VERSION);
    }

    #[test]
    fn run_doctor_deterministic_skips_credential() {
        let dir = tempfile::tempdir().unwrap();
        let report = run_doctor(&DoctorOpts {
            env_kind: EnvKind::Local,
            model: "deterministic".to_owned(),
            output_dir: dir.path().to_path_buf(),
            docker_image: None,
        });
        let cred = report
            .checks
            .iter()
            .find(|c| c.check == "credential")
            .expect("credential check present");
        assert_eq!(cred.status, CheckStatus::Skip);
    }

    #[test]
    fn run_doctor_has_all_five_checks() {
        let dir = tempfile::tempdir().unwrap();
        let report = run_doctor(&DoctorOpts {
            env_kind: EnvKind::Local,
            model: "deterministic".to_owned(),
            output_dir: dir.path().to_path_buf(),
            docker_image: None,
        });
        let ids: Vec<&str> = report.checks.iter().map(|c| c.check.as_str()).collect();
        assert_eq!(
            ids,
            vec!["git", "credential", "docker", "output_dir", "toolchain"]
        );
    }

    // ── docker readiness preconditions (AC#2c) ───────────────────────────

    #[cfg(not(feature = "docker"))]
    #[test]
    fn docker_without_feature_fails() {
        // Built without the docker feature: a docker env is un-runnable.
        let check = check_docker(&EnvKind::Docker, Some("ubuntu:24.04"));
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("feature"));
    }

    #[cfg(feature = "docker")]
    #[test]
    fn docker_with_feature_but_no_image_fails() {
        // Built with the docker feature but no configured image → cannot start.
        let check = check_docker(&EnvKind::Docker, None);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("docker_image"));
    }

    #[test]
    fn docker_skip_for_local_regardless_of_image() {
        let check = check_docker(&EnvKind::Local, None);
        assert_eq!(check.status, CheckStatus::Skip);
    }

    // ── output dir writability (AC#2d) ───────────────────────────────────

    #[test]
    fn output_dir_writable_pass_and_probe_removed() {
        let dir = tempfile::tempdir().unwrap();
        let check = check_output_dir(dir.path());
        assert_eq!(check.status, CheckStatus::Pass);
        // No probe file should remain.
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(leftover.is_empty(), "probe file should be cleaned up");
    }

    #[test]
    fn output_dir_creates_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("runs").join("nested");
        let check = check_output_dir(&nested);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(nested.is_dir());
    }

    // ── ready aggregation ────────────────────────────────────────────────

    #[test]
    fn ready_false_when_any_check_fails() {
        let report = DoctorReport {
            schema_version: DOCTOR_SCHEMA_VERSION,
            ready: true,
            checks: vec![DoctorCheck::pass("a", "ok"), DoctorCheck::fail("b", "bad")],
        };
        // run_doctor computes `ready`; emulate that rule here.
        let ready = report.checks.iter().all(|c| c.status != CheckStatus::Fail);
        assert!(!ready);
    }

    #[test]
    fn skip_does_not_block_ready() {
        let checks = [DoctorCheck::pass("a", "ok"), DoctorCheck::skip("b", "n/a")];
        let ready = checks.iter().all(|c| c.status != CheckStatus::Fail);
        assert!(ready);
    }

    // ── version parsing / comparison (AC#2e) ─────────────────────────────

    #[test]
    fn parse_version_handles_short_and_suffixed() {
        assert_eq!(parse_version("1.85"), Some((1, 85, 0)));
        assert_eq!(parse_version("1.85.0"), Some((1, 85, 0)));
        assert_eq!(parse_version("1.90.1-nightly"), Some((1, 90, 1)));
        assert_eq!(parse_version("1"), Some((1, 0, 0)));
        assert_eq!(parse_version("nope"), None);
    }

    #[test]
    fn version_at_least_compares_triples() {
        assert!(version_at_least((1, 85, 0), (1, 85, 0)));
        assert!(version_at_least((1, 90, 1), (1, 85, 0)));
        assert!(!version_at_least((1, 84, 9), (1, 85, 0)));
        assert!(!version_at_least((0, 99, 0), (1, 0, 0)));
    }

    // ── credential presence (AC#2b / AC#4) ───────────────────────────────

    #[test]
    fn credential_skip_for_deterministic() {
        let check = check_credential("deterministic");
        assert_eq!(check.status, CheckStatus::Skip);
    }

    #[test]
    fn credential_detail_never_contains_value() {
        // Even when present, the detail must only name the var, never its value.
        // Use a uniquely-named var to avoid clobbering real provider keys.
        let model = "weirdprov/model";
        let var = expected_credential_env(model).unwrap();
        // SAFETY: single-threaded test; restore immediately after.
        unsafe { std::env::set_var(&var, "TOPSECRETVALUE") };
        let check = check_credential(model);
        unsafe { std::env::remove_var(&var) };
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(!check.detail.contains("TOPSECRETVALUE"));
    }

    #[test]
    fn credential_fail_when_var_absent() {
        // A uniquely-named provider whose env var is guaranteed unset → fail
        // with a remediation hint naming the variable.
        let model = "zzznoprov/model";
        let var = expected_credential_env(model).unwrap();
        // SAFETY: single-threaded test; ensure the var is absent.
        unsafe { std::env::remove_var(&var) };
        let check = check_credential(model);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains(&var));
        assert!(check.detail.contains("export"));
    }

    // ── output dir failure path (AC#2d) ──────────────────────────────────

    #[test]
    fn output_dir_fail_when_parent_is_a_file() {
        // Point the dir at a child of a regular file; `create_dir_all` fails
        // regardless of uid (robust even when tests run as root).
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let target = blocker.join("runs");
        let check = check_output_dir(&target);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("--output"));
    }

    // ── executable resolution (AC#2a) ────────────────────────────────────

    #[test]
    fn resolve_on_path_none_for_nonexistent() {
        assert!(resolve_on_path("definitely-not-a-real-binary-xyz").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn is_executable_requires_exec_bit_on_unix() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        std::fs::write(&plain, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable(&plain), "non-exec file must not resolve");

        let exec = dir.path().join("exec");
        std::fs::write(&exec, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exec, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable(&exec), "exec file must resolve");

        // A directory is not an executable file.
        assert!(!is_executable(dir.path()));
    }
}

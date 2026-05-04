//! Publish a captured patch artifact as a GitHub pull request.
//!
//! The publisher treats the patch file as the behavior boundary: the branch
//! commit is created by applying that exact patch to the caller's target
//! branch, then pushing a deterministic head branch and opening or reusing a PR.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use crate::error::ConfigError;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::error::Error;

const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishMode {
    Open,
    DryRun,
}

#[derive(Debug, Clone)]
pub struct GithubPrOptions {
    pub target_repo: String,
    pub target_branch: String,
    pub task_id: String,
    pub trajectory_ref: String,
    pub patch_path: PathBuf,
    pub branch_prefix: String,
    pub token_env: String,
    pub mode: PublishMode,
    pub timeout_secs: u64,
    pub max_retries: u32,
    pub backoff_base_ms: u64,
}

#[derive(Debug, Clone)]
pub struct GithubPrSweepConfig {
    pub target_repo: String,
    pub target_branch: String,
    pub token_env: String,
    pub mode: PublishMode,
    pub timeout_secs: u64,
    pub max_retries: u32,
    pub backoff_base_ms: u64,
    pub branch_prefix: String,
}

impl GithubPrSweepConfig {
    pub fn options_for_run(
        &self,
        instance_id: &str,
        run_index: u32,
        trajectory_path: &Path,
        patch_path: &Path,
    ) -> GithubPrOptions {
        let task_id = if run_index == 1 {
            instance_id.to_owned()
        } else {
            format!("{instance_id}-run-{run_index}")
        };
        GithubPrOptions {
            target_repo: self.target_repo.clone(),
            target_branch: self.target_branch.clone(),
            task_id,
            trajectory_ref: trajectory_path.display().to_string(),
            patch_path: patch_path.to_path_buf(),
            branch_prefix: self.branch_prefix.clone(),
            token_env: self.token_env.clone(),
            mode: self.mode,
            timeout_secs: self.timeout_secs,
            max_retries: self.max_retries,
            backoff_base_ms: self.backoff_base_ms,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestPlan {
    pub target_repo: String,
    pub base_branch: String,
    pub head_branch: String,
    pub title: String,
    pub body: String,
    pub trajectory_ref: String,
    pub patch_path: PathBuf,
    pub summary: PatchSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchSummary {
    pub files_changed: usize,
    pub additions: usize,
    pub deletions: usize,
    pub files: Vec<String>,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishResult {
    pub plan: PullRequestPlan,
    pub url: Option<String>,
    pub dry_run_output: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PullRequestResponse {
    html_url: String,
}

#[derive(Debug, Serialize)]
struct CreatePullRequestRequest<'a> {
    title: &'a str,
    head: &'a str,
    base: &'a str,
    body: &'a str,
    maintainer_can_modify: bool,
}

#[derive(Debug, Clone)]
struct RepoParts {
    owner: String,
    name: String,
}

pub fn build_pr_plan(
    options: &GithubPrOptions,
    patch_text: &str,
) -> Result<PullRequestPlan, Error> {
    let repo = parse_repo(&options.target_repo)?;
    let task_id = options.task_id.trim();
    if task_id.is_empty() {
        return Err(Error::Github(
            "task id is required for PR publishing".into(),
        ));
    }
    let base_branch = validate_branch_component(&options.target_branch, "target branch")?;
    let prefix = normalize_branch_prefix(&options.branch_prefix).map_err(Error::Github)?;
    let task_slug = task_branch_component(task_id);
    let head_branch = format!("{prefix}/{task_slug}");
    let summary = summarize_patch(patch_text);
    let title = format!("rust-swe-agent: {task_id}");
    let body = render_pr_body(
        task_id,
        &options.trajectory_ref,
        &options.patch_path,
        &summary,
    );

    Ok(PullRequestPlan {
        target_repo: format!("{}/{}", repo.owner, repo.name),
        base_branch,
        head_branch,
        title,
        body,
        trajectory_ref: options.trajectory_ref.clone(),
        patch_path: options.patch_path.clone(),
        summary,
    })
}

pub fn render_dry_run(plan: &PullRequestPlan) -> String {
    let mut out = String::new();
    out.push_str("github_pr_dry_run:\n");
    let _ = writeln!(out, "target_repo: {}", plan.target_repo);
    let _ = writeln!(out, "base: {}", plan.base_branch);
    let _ = writeln!(out, "head: {}", plan.head_branch);
    let _ = writeln!(out, "title: {}", plan.title);
    out.push_str("body:\n");
    for line in plan.body.lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("patch_summary:\n");
    let _ = writeln!(out, "  files_changed: {}", plan.summary.files_changed);
    let _ = writeln!(out, "  additions: {}", plan.summary.additions);
    let _ = writeln!(out, "  deletions: {}", plan.summary.deletions);
    let _ = writeln!(out, "  bytes: {}", plan.summary.bytes);
    for file in &plan.summary.files {
        let _ = writeln!(out, "  - {file}");
    }
    out
}

pub async fn publish(options: GithubPrOptions) -> Result<PublishResult, Error> {
    let patch_text = std::fs::read_to_string(&options.patch_path).map_err(|e| {
        Error::Github(format!(
            "failed to read patch `{}`: {e}",
            options.patch_path.display()
        ))
    })?;
    let plan = build_pr_plan(&options, &patch_text)?;
    if options.mode == PublishMode::DryRun {
        return Ok(PublishResult {
            dry_run_output: Some(render_dry_run(&plan)),
            plan,
            url: None,
        });
    }
    if patch_text.is_empty() {
        return Err(Error::Github(format!(
            "refusing to open PR for empty patch `{}`",
            options.patch_path.display()
        )));
    }

    let timeout = Duration::from_secs(options.timeout_secs.max(1));
    let deadline = tokio::time::Instant::now() + timeout;
    let token = read_github_token(&options.token_env)?;
    push_patch_branch(&plan, &patch_text, &token, deadline).await?;

    let api = GithubApiClient::new(token, options.max_retries, options.backoff_base_ms)?;
    let url = tokio::time::timeout_at(deadline, api.ensure_pull_request(&plan))
        .await
        .map_err(|_| {
            Error::Github(format!("timed out after {}s opening PR", timeout.as_secs()))
        })??;

    Ok(PublishResult {
        plan,
        url: Some(url),
        dry_run_output: None,
    })
}

fn render_pr_body(
    task_id: &str,
    trajectory_ref: &str,
    patch_path: &Path,
    summary: &PatchSummary,
) -> String {
    let files = if summary.files.is_empty() {
        "- (no files detected in patch)\n".to_owned()
    } else {
        let mut files = String::new();
        for file in &summary.files {
            let _ = writeln!(files, "- `{file}`");
        }
        files
    };
    format!(
        "Automated patch proposed by `rust-swe-agent` for `{task_id}`.\n\n\
         Trajectory: `{trajectory_ref}`\n\n\
         Patch artifact: `{}`\n\n\
         Patch summary:\n\
         - files changed: {}\n\
         - additions: {}\n\
         - deletions: {}\n\n\
         Files:\n{files}",
        patch_path.display(),
        summary.files_changed,
        summary.additions,
        summary.deletions
    )
}

fn summarize_patch(patch_text: &str) -> PatchSummary {
    let mut files = BTreeSet::new();
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for line in patch_text.lines() {
        if let Some(file) = parse_diff_file(line) {
            files.insert(file);
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            additions += 1;
        } else if line.starts_with('-') {
            deletions += 1;
        }
    }
    PatchSummary {
        files_changed: files.len(),
        additions,
        deletions,
        files: files.into_iter().collect(),
        bytes: patch_text.len(),
    }
}

fn parse_diff_file(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    let mut parts = rest.split_whitespace();
    let _left = parts.next()?;
    let right = parts.next()?;
    Some(
        right
            .strip_prefix("b/")
            .unwrap_or(right)
            .trim_matches('"')
            .to_owned(),
    )
}

fn read_github_token(token_env: &str) -> Result<String, Error> {
    let env_name = token_env.trim();
    if env_name.is_empty() {
        return Err(Error::Github(
            "--github-token-env must name a non-empty environment variable".into(),
        ));
    }
    let token = std::env::var(env_name)
        .or_else(|_| {
            if env_name == "GITHUB_TOKEN" {
                std::env::var("GH_TOKEN")
            } else {
                Err(std::env::VarError::NotPresent)
            }
        })
        .map_err(|_| {
            Error::Github(format!(
                "missing GitHub token: set `{env_name}` to a PAT or GitHub App installation token"
            ))
        })?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(Error::Github(format!(
            "GitHub token env `{env_name}` is empty"
        )));
    }
    Ok(token)
}

async fn push_patch_branch(
    plan: &PullRequestPlan,
    patch_text: &str,
    token: &str,
    deadline: tokio::time::Instant,
) -> Result<(), Error> {
    let work = create_temp_workdir()?;
    let repo_url = authed_repo_url(&plan.target_repo, token);
    run_git(work.path(), &["init", "-q"], token, deadline).await?;
    run_git(
        work.path(),
        &["config", "user.email", "rust-swe-agent@example.invalid"],
        token,
        deadline,
    )
    .await?;
    run_git(
        work.path(),
        &["config", "user.name", "rust-swe-agent"],
        token,
        deadline,
    )
    .await?;
    run_git(
        work.path(),
        &["config", "commit.gpgsign", "false"],
        token,
        deadline,
    )
    .await?;
    run_git(
        work.path(),
        &["remote", "add", "origin", &repo_url],
        token,
        deadline,
    )
    .await?;
    run_git(
        work.path(),
        &["fetch", "--depth=1", "origin", &plan.base_branch],
        token,
        deadline,
    )
    .await?;
    fetch_existing_head_branch(work.path(), &plan.head_branch, token, deadline).await?;
    run_git(
        work.path(),
        &["checkout", "-B", &plan.head_branch, "FETCH_HEAD"],
        token,
        deadline,
    )
    .await?;

    let patch_path = work.path().join("agent.patch");
    std::fs::write(&patch_path, patch_text)?;
    run_git(
        work.path(),
        &[
            "apply",
            "--binary",
            patch_path
                .to_str()
                .ok_or_else(|| Error::Github("patch path is not valid UTF-8".into()))?,
        ],
        token,
        deadline,
    )
    .await?;
    let status = run_git_capture(work.path(), &["status", "--porcelain"], token, deadline).await?;
    if String::from_utf8_lossy(&status.stdout).trim().is_empty() {
        return Err(Error::Github(
            "patch applied but produced no commit changes".into(),
        ));
    }
    run_git(work.path(), &["add", "-A"], token, deadline).await?;
    run_git(work.path(), &["commit", "-m", &plan.title], token, deadline).await?;
    let refspec = format!("HEAD:refs/heads/{}", plan.head_branch);
    run_git(
        work.path(),
        &["push", "--force-with-lease", "origin", &refspec],
        token,
        deadline,
    )
    .await?;
    Ok(())
}

async fn fetch_existing_head_branch(
    cwd: &Path,
    head_branch: &str,
    token: &str,
    deadline: tokio::time::Instant,
) -> Result<(), Error> {
    let remote_ref = format!("refs/heads/{head_branch}");
    let tracking_ref = format!("refs/remotes/origin/{head_branch}");
    let refspec = format!("{remote_ref}:{tracking_ref}");
    let args = ["fetch", "--depth=1", "origin", refspec.as_str()];
    let output = run_git_capture(cwd, &args, token, deadline).await?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    if is_missing_remote_ref(&stderr) || is_missing_remote_ref(&stdout) {
        return Ok(());
    }

    let stderr = sanitize_secret(&stderr, token);
    let stdout = sanitize_secret(&stdout, token);
    Err(Error::Github(format!(
        "git {} failed: {}\n{}",
        sanitized_args(&args, token),
        stderr.trim(),
        stdout.trim()
    )))
}

fn is_missing_remote_ref(output: &str) -> bool {
    let output = output.to_ascii_lowercase();
    output.contains("couldn't find remote ref") || output.contains("could not find remote ref")
}

async fn run_git(
    cwd: &Path,
    args: &[&str],
    token: &str,
    deadline: tokio::time::Instant,
) -> Result<(), Error> {
    let output = run_git_capture(cwd, args, token, deadline).await?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = sanitize_secret(&String::from_utf8_lossy(&output.stderr), token);
    let stdout = sanitize_secret(&String::from_utf8_lossy(&output.stdout), token);
    Err(Error::Github(format!(
        "git {} failed: {}\n{}",
        sanitized_args(args, token),
        stderr.trim(),
        stdout.trim()
    )))
}

async fn run_git_capture(
    cwd: &Path,
    args: &[&str],
    token: &str,
    deadline: tokio::time::Instant,
) -> Result<Output, Error> {
    run_process_capture("git", cwd, args, token, deadline).await
}

async fn run_process_capture(
    program: &str,
    cwd: &Path,
    args: &[&str],
    token: &str,
    deadline: tokio::time::Instant,
) -> Result<Output, Error> {
    let command = command_name(program, args, token);
    let mut child = Command::new(program);
    child
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .kill_on_drop(true);

    tokio::time::timeout_at(deadline, child.output())
        .await
        .map_err(|_| Error::Github(format!("{command} timed out")))?
        .map_err(|e| Error::Github(format!("failed to run {command}: {e}")))
}

fn command_name(program: &str, args: &[&str], token: &str) -> String {
    let mut command = String::from(program);
    let args = sanitized_args(args, token);
    if !args.is_empty() {
        command.push(' ');
        command.push_str(&args);
    }
    command
}

fn sanitized_args(args: &[&str], token: &str) -> String {
    args.iter()
        .map(|arg| sanitize_secret(arg, token))
        .collect::<Vec<_>>()
        .join(" ")
}

fn sanitize_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        value.to_owned()
    } else {
        value.replace(secret, "<redacted>")
    }
}

fn authed_repo_url(target_repo: &str, token: &str) -> String {
    format!(
        "https://x-access-token:{}@github.com/{target_repo}.git",
        percent_encode(token)
    )
}

fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn create_temp_workdir() -> Result<tempfile::TempDir, Error> {
    tempfile::Builder::new()
        .prefix("rust-swe-agent-github-pr-")
        .tempdir()
        .map_err(Error::from)
}

struct GithubApiClient {
    http: reqwest::Client,
    token: String,
    max_retries: u32,
    backoff_base: Duration,
}

impl GithubApiClient {
    fn new(token: String, max_retries: u32, backoff_base_ms: u64) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .user_agent("rust-swe-agent")
            .build()
            .map_err(|e| Error::Github(format!("failed to build GitHub HTTP client: {e}")))?;
        Ok(Self {
            http,
            token,
            max_retries,
            backoff_base: Duration::from_millis(backoff_base_ms.max(1)),
        })
    }

    async fn ensure_pull_request(&self, plan: &PullRequestPlan) -> Result<String, Error> {
        if let Some(url) = self.find_open_pull_request(plan).await? {
            return Ok(url);
        }
        match self.create_pull_request(plan).await {
            Ok(url) => Ok(url),
            Err(err) if err.to_string().contains("422") => {
                if let Some(url) = self.find_open_pull_request(plan).await? {
                    Ok(url)
                } else {
                    Err(err)
                }
            }
            Err(err) => Err(err),
        }
    }

    async fn find_open_pull_request(
        &self,
        plan: &PullRequestPlan,
    ) -> Result<Option<String>, Error> {
        let repo = parse_repo(&plan.target_repo)?;
        let head = format!("{}:{}", repo.owner, plan.head_branch);
        let url = format!(
            "{DEFAULT_GITHUB_API_URL}/repos/{}/{}/pulls?state=open&head={}&base={}",
            repo.owner,
            repo.name,
            percent_encode(&head),
            percent_encode(&plan.base_branch)
        );
        let prs: Vec<PullRequestResponse> = self
            .send_json(|| self.http.get(&url).headers(self.headers()))
            .await?;
        Ok(prs.into_iter().next().map(|pr| pr.html_url))
    }

    async fn create_pull_request(&self, plan: &PullRequestPlan) -> Result<String, Error> {
        let repo = parse_repo(&plan.target_repo)?;
        let url = format!(
            "{DEFAULT_GITHUB_API_URL}/repos/{}/{}/pulls",
            repo.owner, repo.name
        );
        let payload = CreatePullRequestRequest {
            title: &plan.title,
            head: &plan.head_branch,
            base: &plan.base_branch,
            body: &plan.body,
            maintainer_can_modify: true,
        };
        let pr: PullRequestResponse = self
            .send_json(|| self.http.post(&url).headers(self.headers()).json(&payload))
            .await?;
        Ok(pr.html_url)
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};

        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );
        let auth = format!("Bearer {}", self.token);
        if let Ok(value) = HeaderValue::from_str(&auth) {
            headers.insert(AUTHORIZATION, value);
        }
        headers
    }

    async fn send_json<T, F>(&self, mut build: F) -> Result<T, Error>
    where
        T: DeserializeOwned,
        F: FnMut() -> reqwest::RequestBuilder,
    {
        let mut attempt = 0u32;
        loop {
            let response = build()
                .send()
                .await
                .map_err(|e| Error::Github(format!("GitHub API request failed: {e}")))?;
            let status = response.status();
            if status.is_success() {
                return response.json::<T>().await.map_err(|e| {
                    Error::Github(format!("GitHub API returned malformed JSON: {e}"))
                });
            }
            let retry_delay = retry_delay_for_response(&response, self.backoff_for(attempt));
            let body = response.text().await.unwrap_or_default();
            if attempt < self.max_retries {
                if let Some(delay) = retry_delay {
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                }
            }
            return Err(Error::Github(format!(
                "GitHub API status {status}: {}",
                sanitize_secret(&body, &self.token)
            )));
        }
    }

    fn backoff_for(&self, attempt: u32) -> Duration {
        let multiplier = 1u64.checked_shl(attempt.min(10)).unwrap_or(1024);
        self.backoff_base
            .saturating_mul(u32::try_from(multiplier).unwrap_or(u32::MAX))
    }
}

fn retry_delay_for_response(response: &reqwest::Response, fallback: Duration) -> Option<Duration> {
    let status = response.status();
    if !(status.as_u16() == 429
        || status.is_server_error()
        || (status.as_u16() == 403
            && response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                == Some("0")))
    {
        return None;
    }
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .or_else(|| Some(fallback.min(Duration::from_secs(5))))
}

fn parse_repo(raw: &str) -> Result<RepoParts, Error> {
    let trimmed = raw.trim();
    let mut parts = trimmed.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return Err(Error::Github(format!(
            "target repo must be `owner/name`, got `{raw}`"
        )));
    }
    Ok(RepoParts {
        owner: owner.to_owned(),
        name: name.to_owned(),
    })
}

pub fn validate_branch_prefix(raw: &str) -> Result<(), ConfigError> {
    normalize_branch_prefix(raw)
        .map(|_| ())
        .map_err(ConfigError::Invalid)
}

fn normalize_branch_prefix(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err("branch prefix must not be empty".into());
    }
    let slug = slug_path(trimmed);
    if slug.is_empty() {
        return Err(format!(
            "branch prefix `{raw}` must slug to at least one ASCII alphanumeric character"
        ));
    }
    Ok(slug)
}

fn validate_branch_component(raw: &str, label: &str) -> Result<String, Error> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') || trimmed.contains("..") {
        return Err(Error::Github(format!("invalid {label}: `{raw}`")));
    }
    Ok(trimmed.to_owned())
}

fn task_branch_component(raw: &str) -> String {
    let slug = slug_for_branch(raw);
    let digest = short_task_id_hash(raw);
    format!("{slug}-{digest}")
}

fn slug_for_branch(raw: &str) -> String {
    let slug = slug_path(raw);
    if slug.is_empty() { "task".into() } else { slug }
}

fn short_task_id_hash(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    let mut out = String::with_capacity(32);
    for byte in &digest[..16] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn slug_path(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut previous_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            out.push('-');
            previous_dash = true;
        }
    }
    out.trim_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn percent_encode_escapes_token_delimiters() {
        assert_eq!(percent_encode("abc:def@x/y"), "abc%3Adef%40x%2Fy");
    }

    #[test]
    fn repo_parser_requires_owner_and_name() {
        assert!(parse_repo("madmax983/rust_swe_agent").is_ok());
        assert!(parse_repo("rust_swe_agent").is_err());
        assert!(parse_repo("a/b/c").is_err());
    }

    #[test]
    fn sanitizer_removes_token_from_git_errors() {
        let token = "GITHUB_TOKEN_VALUE";
        assert_eq!(
            sanitize_secret(
                "https://x-access-token:GITHUB_TOKEN_VALUE@github.com/o/r.git",
                token
            ),
            "https://x-access-token:<redacted>@github.com/o/r.git"
        );
    }

    #[test]
    fn temp_workdirs_are_unique_and_created_by_tempfile() {
        let first = create_temp_workdir().unwrap();
        let second = create_temp_workdir().unwrap();

        assert_ne!(first.path(), second.path());
        assert!(first.path().exists());
        assert!(second.path().exists());
    }

    #[test]
    fn missing_remote_ref_detection_accepts_git_wording() {
        assert!(is_missing_remote_ref(
            "fatal: couldn't find remote ref refs/heads/rust-swe-agent/task"
        ));
        assert!(is_missing_remote_ref(
            "fatal: could not find remote ref refs/heads/rust-swe-agent/task"
        ));
        assert!(!is_missing_remote_ref("fatal: authentication failed"));
    }

    #[tokio::test]
    async fn process_capture_timeout_terminates_child() {
        let work = create_temp_workdir().unwrap();
        let marker = work.path().join("timeout-marker.txt");
        let marker_arg = marker.display().to_string();
        #[cfg(windows)]
        let (program, args): (&str, Vec<&str>) = (
            "powershell",
            vec![
                "-NoProfile",
                "-Command",
                "Start-Sleep -Milliseconds 700; Set-Content -LiteralPath $args[0] -Value done",
                &marker_arg,
            ],
        );
        #[cfg(not(windows))]
        let (program, args): (&str, Vec<&str>) = (
            "sh",
            vec!["-c", "sleep 0.7; printf done > \"$1\"", "sh", &marker_arg],
        );

        let deadline = tokio::time::Instant::now() + Duration::from_millis(50);
        let err = run_process_capture(program, work.path(), &args, "secret", deadline)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("timed out"), "{err}");
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(!marker.exists(), "timed-out child still wrote side effect");
    }

    #[tokio::test]
    async fn fetches_existing_head_branch_before_force_with_lease_push() {
        let root = create_temp_workdir().unwrap();
        let remote = root.path().join("remote.git");
        let seed = root.path().join("seed");
        let work = root.path().join("work");
        let token = "secret";
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let head_branch = "rust-swe-agent/existing-task";
        let head_refspec = format!("HEAD:refs/heads/{head_branch}");

        let remote_arg = seed_remote_with_existing_head(
            root.path(),
            &seed,
            &remote,
            head_branch,
            token,
            deadline,
        )
        .await;
        prepare_publish_worktree(&work, &remote_arg, head_branch, token, deadline).await;

        let stale_push = run_git_capture(
            &work,
            &["push", "--force-with-lease", "origin", &head_refspec],
            token,
            deadline,
        )
        .await
        .unwrap();
        assert!(
            !stale_push.status.success(),
            "push unexpectedly succeeded without origin/{head_branch}"
        );

        fetch_existing_head_branch(&work, head_branch, token, deadline)
            .await
            .unwrap();
        git(
            &work,
            &["push", "--force-with-lease", "origin", &head_refspec],
            token,
            deadline,
        )
        .await;
    }

    async fn seed_remote_with_existing_head(
        root: &Path,
        seed: &Path,
        remote: &Path,
        head_branch: &str,
        token: &str,
        deadline: tokio::time::Instant,
    ) -> String {
        std::fs::create_dir_all(seed).unwrap();
        let remote_arg = git_path(remote);
        let head_refspec = format!("HEAD:refs/heads/{head_branch}");

        git(root, &["init", "--bare", &remote_arg], token, deadline).await;
        git(seed, &["init", "-q"], token, deadline).await;
        configure_git_author(seed, token, deadline).await;
        std::fs::write(seed.join("file.txt"), "base\n").unwrap();
        git(seed, &["add", "file.txt"], token, deadline).await;
        git(seed, &["commit", "-q", "-m", "base"], token, deadline).await;
        git(seed, &["branch", "-M", "main"], token, deadline).await;
        git(
            seed,
            &["remote", "add", "origin", &remote_arg],
            token,
            deadline,
        )
        .await;
        git(
            seed,
            &["push", "origin", "HEAD:refs/heads/main"],
            token,
            deadline,
        )
        .await;

        git(
            seed,
            &["checkout", "-q", "-B", head_branch],
            token,
            deadline,
        )
        .await;
        std::fs::write(seed.join("file.txt"), "existing head\n").unwrap();
        git(seed, &["add", "file.txt"], token, deadline).await;
        git(
            seed,
            &["commit", "-q", "-m", "existing head"],
            token,
            deadline,
        )
        .await;
        git(seed, &["push", "origin", &head_refspec], token, deadline).await;
        remote_arg
    }

    async fn prepare_publish_worktree(
        work: &Path,
        remote_arg: &str,
        head_branch: &str,
        token: &str,
        deadline: tokio::time::Instant,
    ) {
        std::fs::create_dir_all(work).unwrap();
        git(work, &["init", "-q"], token, deadline).await;
        configure_git_author(work, token, deadline).await;
        git(
            work,
            &["remote", "add", "origin", remote_arg],
            token,
            deadline,
        )
        .await;
        git(
            work,
            &["fetch", "--depth=1", "origin", "main"],
            token,
            deadline,
        )
        .await;
        git(
            work,
            &["checkout", "-q", "-B", head_branch, "FETCH_HEAD"],
            token,
            deadline,
        )
        .await;
        std::fs::write(work.join("file.txt"), "new publish\n").unwrap();
        git(work, &["add", "file.txt"], token, deadline).await;
        git(
            work,
            &["commit", "-q", "-m", "new publish"],
            token,
            deadline,
        )
        .await;
    }

    async fn configure_git_author(cwd: &Path, token: &str, deadline: tokio::time::Instant) {
        git(
            cwd,
            &["config", "user.email", "test@example.invalid"],
            token,
            deadline,
        )
        .await;
        git(cwd, &["config", "user.name", "test"], token, deadline).await;
        git(cwd, &["config", "commit.gpgsign", "false"], token, deadline).await;
    }

    async fn git(cwd: &Path, args: &[&str], token: &str, deadline: tokio::time::Instant) {
        run_git(cwd, args, token, deadline).await.unwrap();
    }

    fn git_path(path: &Path) -> String {
        path.display().to_string().replace('\\', "/")
    }
}

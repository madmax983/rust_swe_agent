use crate::error::{Error, GithubIssueError};
use crate::prompt_guard::{PromptGuard, UntrustedKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueProvenance {
    pub issue_repo: Option<String>,
    pub issue_number: Option<u64>,
    pub issue_fetched_at_utc: Option<String>,
    pub issue_body_sha256: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GithubIssueSnapshot {
    pub title: String,
    pub body: Option<String>,
    pub comments: Option<Vec<GithubIssueComment>>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GithubIssueComment {
    pub body: Option<String>,
    pub author: Option<GithubIssueCommentAuthor>,
    pub user: Option<GithubIssueCommentAuthor>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GithubIssueCommentAuthor {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct GithubIssueOnline {
    pub title: String,
    pub body: Option<String>,
}

/// Parses a GitHub issue reference (`owner/repo#123`) or issue URL
/// (`https://github.com/owner/repo/issues/123`).
pub fn parse_issue_ref(input: &str) -> Result<IssueRef, Error> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Usage(
            "empty issue reference".into(),
        )));
    }

    let input_lower = input.to_ascii_lowercase();
    if input_lower.starts_with("http://") || input_lower.starts_with("https://") {
        let url = reqwest::Url::parse(input).map_err(|e| {
            Error::Config(crate::error::ConfigError::Usage(format!(
                "invalid issue URL: {e}"
            )))
        })?;

        let host = url.host_str().ok_or_else(|| {
            Error::Config(crate::error::ConfigError::Usage("URL has no host".into()))
        })?;
        if !host.eq_ignore_ascii_case("github.com") {
            return Err(Error::Config(crate::error::ConfigError::Usage(format!(
                "unsupported forge: {host}; only github.com is supported"
            ))));
        }

        let segments: Vec<&str> = url
            .path_segments()
            .ok_or_else(|| {
                Error::Config(crate::error::ConfigError::Usage("invalid URL path".into()))
            })?
            .filter(|s| !s.is_empty())
            .collect();

        if segments.len() != 4 || !segments[2].eq_ignore_ascii_case("issues") {
            return Err(Error::Config(crate::error::ConfigError::Usage(
                "invalid GitHub issue URL path; expected /owner/repo/issues/number".into(),
            )));
        }

        let owner = segments[0].to_owned();
        let repo = segments[1].to_owned();
        let number = segments[3].parse::<u64>().map_err(|_| {
            Error::Config(crate::error::ConfigError::Usage(
                "invalid issue number in URL".into(),
            ))
        })?;

        return Ok(IssueRef {
            owner,
            repo,
            number,
        });
    }

    let parts: Vec<&str> = input.split('#').collect();
    if parts.len() != 2 {
        return Err(Error::Config(crate::error::ConfigError::Usage(
            "invalid issue reference format; expected owner/repo#number or GitHub URL".into(),
        )));
    }

    let repo_parts: Vec<&str> = parts[0].split('/').collect();
    if repo_parts.len() != 2 || repo_parts[0].is_empty() || repo_parts[1].is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Usage(
            "invalid repository format in issue reference; expected owner/repo#number".into(),
        )));
    }

    let owner = repo_parts[0].to_owned();
    let repo = repo_parts[1].to_owned();
    let number = parts[1].parse::<u64>().map_err(|_| {
        Error::Config(crate::error::ConfigError::Usage(
            "invalid issue number in reference".into(),
        ))
    })?;

    Ok(IssueRef {
        owner,
        repo,
        number,
    })
}

fn read_github_token(token_env: &str) -> Result<String, Error> {
    let env_name = token_env.trim();
    if env_name.is_empty() {
        return Err(Error::Config(crate::error::ConfigError::Usage(
            "--github-token-env must name a non-empty environment variable".into(),
        )));
    }
    let token = std::env::var(env_name)
        .or_else(|_| {
            if env_name == "GITHUB_TOKEN" {
                std::env::var("GH_TOKEN")
            } else {
                Err(std::env::VarError::NotPresent)
            }
        })
        .map_err(|_| Error::GithubIssue(GithubIssueError::MissingToken(env_name.to_owned())))?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Err(Error::GithubIssue(GithubIssueError::MissingToken(
            env_name.to_owned(),
        )));
    }
    Ok(token)
}

fn compute_body_sha256(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Formats the issue components into the final task prompt, wrapping untrusted text in PromptGuard envelopes.
pub fn format_issue_prompt(title: &str, body: &str, comments: &[GithubIssueComment]) -> String {
    let mut out = String::new();
    out.push_str("GitHub Issue: ");
    out.push_str(&PromptGuard::wrap(UntrustedKind::TaskText, title));
    out.push_str("\n\n");

    out.push_str("Issue Description:\n");
    out.push_str(&PromptGuard::wrap(UntrustedKind::TaskText, body));
    out.push_str("\n\n");

    if !comments.is_empty() {
        out.push_str("Discussion Comments:\n");
        for c in comments {
            let author = c
                .author
                .as_ref()
                .or(c.user.as_ref())
                .map_or("anonymous", |a| a.login.as_str());
            let comment_body = c.body.as_deref().unwrap_or("");
            let comment_block = format!("{author}:\n{comment_body}");
            out.push_str(&PromptGuard::wrap(UntrustedKind::TaskText, &comment_block));
            out.push_str("\n\n");
        }
    }
    out
}

fn handle_api_error(status: reqwest::StatusCode, body: &str) -> Error {
    if status == reqwest::StatusCode::NOT_FOUND {
        Error::GithubIssue(GithubIssueError::NotFound(body.to_owned()))
    } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || (status == reqwest::StatusCode::FORBIDDEN
            && (body.contains("rate limit")
                || body.contains("rate-limit")
                || body.contains("api-rate-limit-exceeded")))
    {
        Error::GithubIssue(GithubIssueError::RateLimited(body.to_owned()))
    } else if status == reqwest::StatusCode::FORBIDDEN {
        Error::GithubIssue(GithubIssueError::NotFound(body.to_owned()))
    } else {
        Error::GithubIssue(GithubIssueError::RequestFailed(format!(
            "status {status}: {body}"
        )))
    }
}

/// Resolves a GitHub issue task online or from a local JSON snapshot.
#[allow(clippy::too_many_lines)]
pub async fn resolve_issue_task_async(
    issue_ref_str: Option<String>,
    snapshot_path: Option<PathBuf>,
    token_env: &str,
) -> Result<(String, IssueProvenance), Error> {
    if let Some(path) = snapshot_path {
        let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
            Error::Config(crate::error::ConfigError::Usage(format!(
                "failed to read snapshot file: {e}"
            )))
        })?;
        let snapshot: GithubIssueSnapshot = serde_json::from_str(&content).map_err(|e| {
            Error::Config(crate::error::ConfigError::Usage(format!(
                "failed to parse snapshot JSON: {e}"
            )))
        })?;

        let snapshot_body = snapshot.body.as_deref().unwrap_or("");
        let body_hash = compute_body_sha256(snapshot_body);
        let comments = snapshot.comments.unwrap_or_default();
        let task_prompt = format_issue_prompt(&snapshot.title, snapshot_body, &comments);

        let prov = IssueProvenance {
            issue_repo: None,
            issue_number: None,
            issue_fetched_at_utc: None,
            issue_body_sha256: Some(body_hash),
        };

        return Ok((task_prompt, prov));
    }

    if let Some(ref_str) = issue_ref_str {
        let issue_ref = parse_issue_ref(&ref_str)?;
        let token = read_github_token(token_env)?;

        let client = reqwest::Client::builder()
            .user_agent("max")
            .build()
            .map_err(|e| {
                Error::GithubIssue(GithubIssueError::RequestFailed(format!(
                    "failed to build HTTP client: {e}"
                )))
            })?;

        let fetched_at = chrono::Utc::now().to_rfc3339();

        // 1. Fetch issue body
        let issue_url = format!(
            "https://api.github.com/repos/{}/{}/issues/{}",
            issue_ref.owner, issue_ref.repo, issue_ref.number
        );
        let res = client
            .get(&issue_url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .map_err(|e| {
                Error::GithubIssue(GithubIssueError::RequestFailed(format!(
                    "failed to send issue request: {e}"
                )))
            })?;

        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(handle_api_error(status, &body));
        }

        let issue_data: GithubIssueOnline = serde_json::from_str(&body).map_err(|e| {
            Error::GithubIssue(GithubIssueError::RequestFailed(format!(
                "failed to parse issue JSON: {e}"
            )))
        })?;

        // 2. Fetch comments with pagination
        let mut comments = Vec::new();
        let mut page = 1;
        loop {
            let comments_url = format!(
                "https://api.github.com/repos/{}/{}/issues/{}/comments?per_page=100&page={}",
                issue_ref.owner, issue_ref.repo, issue_ref.number, page
            );
            let res_comments = client
                .get(&comments_url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .map_err(|e| {
                    Error::GithubIssue(GithubIssueError::RequestFailed(format!(
                        "failed to send comments request on page {page}: {e}"
                    )))
                })?;

            let status_comments = res_comments.status();
            let comments_body = res_comments.text().await.unwrap_or_default();

            if !status_comments.is_success() {
                return Err(handle_api_error(status_comments, &comments_body));
            }

            let page_comments: Vec<GithubIssueComment> = serde_json::from_str(&comments_body)
                .map_err(|e| {
                    Error::GithubIssue(GithubIssueError::RequestFailed(format!(
                        "failed to parse comments JSON on page {page}: {e}"
                    )))
                })?;

            let page_len = page_comments.len();
            comments.extend(page_comments);

            if page_len < 100 {
                break;
            }
            page += 1;
        }

        let issue_body = issue_data.body.as_deref().unwrap_or("");
        let body_hash = compute_body_sha256(issue_body);
        let task_prompt = format_issue_prompt(&issue_data.title, issue_body, &comments);

        let prov = IssueProvenance {
            issue_repo: Some(format!("{}/{}", issue_ref.owner, issue_ref.repo)),
            issue_number: Some(issue_ref.number),
            issue_fetched_at_utc: Some(fetched_at),
            issue_body_sha256: Some(body_hash),
        };

        return Ok((task_prompt, prov));
    }

    Err(Error::Config(crate::error::ConfigError::Usage(
        "neither issue reference nor snapshot file was provided".into(),
    )))
}

/// Synchronous wrapper for resolving issue task (primarily for tests).
pub fn resolve_issue_task(
    issue_ref_str: Option<String>,
    snapshot_path: Option<PathBuf>,
) -> Result<(String, IssueProvenance), Error> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            Error::Config(crate::error::ConfigError::Usage(format!(
                "failed to build tokio runtime: {e}"
            )))
        })?
        .block_on(resolve_issue_task_async(
            issue_ref_str,
            snapshot_path,
            "GITHUB_TOKEN",
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn test_handle_api_error_mappings() {
        // 404 -> NotFound
        let err = handle_api_error(StatusCode::NOT_FOUND, "not found msg");
        match err {
            Error::GithubIssue(GithubIssueError::NotFound(msg)) => {
                assert_eq!(msg, "not found msg");
            }
            _ => panic!("expected NotFound, got {err:?}"),
        }

        // 403 rate limit -> RateLimited
        let err = handle_api_error(StatusCode::FORBIDDEN, "rate limit exceeded");
        match err {
            Error::GithubIssue(GithubIssueError::RateLimited(msg)) => {
                assert_eq!(msg, "rate limit exceeded");
            }
            _ => panic!("expected RateLimited, got {err:?}"),
        }

        // 403 other -> NotFound
        let err = handle_api_error(StatusCode::FORBIDDEN, "some other 403 error");
        match err {
            Error::GithubIssue(GithubIssueError::NotFound(msg)) => {
                assert_eq!(msg, "some other 403 error");
            }
            _ => panic!("expected NotFound, got {err:?}"),
        }

        // 429 -> RateLimited
        let err = handle_api_error(StatusCode::TOO_MANY_REQUESTS, "too many requests");
        match err {
            Error::GithubIssue(GithubIssueError::RateLimited(msg)) => {
                assert_eq!(msg, "too many requests");
            }
            _ => panic!("expected RateLimited, got {err:?}"),
        }

        // 500 -> RequestFailed
        let err = handle_api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal server error");
        match err {
            Error::GithubIssue(GithubIssueError::RequestFailed(msg)) => {
                assert!(msg.contains("status 500 Internal Server Error: internal server error"));
            }
            _ => panic!("expected RequestFailed, got {err:?}"),
        }
    }
}

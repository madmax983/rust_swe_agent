#![allow(clippy::unwrap_used)]

use maxwells_daemon::run::github_issue::{
    GithubIssueComment, GithubIssueCommentAuthor, format_issue_prompt, parse_issue_ref,
    resolve_issue_task,
};
use std::path::PathBuf;

#[test]
fn test_parse_issue_ref_valid() {
    let cases = vec![
        ("owner/repo#123", "owner", "repo", 123),
        ("owner/repo-name#456", "owner", "repo-name", 456),
        (
            "https://github.com/owner/repo/issues/123",
            "owner",
            "repo",
            123,
        ),
        (
            "http://github.com/owner/repo/issues/123/",
            "owner",
            "repo",
            123,
        ),
        (
            "HTTPS://GITHUB.COM/owner/repo/issues/123",
            "owner",
            "repo",
            123,
        ),
    ];

    for (input, expected_owner, expected_repo, expected_num) in cases {
        let parsed = parse_issue_ref(input).unwrap();
        assert_eq!(parsed.owner, expected_owner);
        assert_eq!(parsed.repo, expected_repo);
        assert_eq!(parsed.number, expected_num);
    }
}

#[test]
fn test_parse_issue_ref_invalid() {
    let cases = vec![
        "owner/repo",
        "owner/repo#abc",
        "owner#123",
        "https://github.com/owner/repo/pull/123",
        "https://other.com/owner/repo/issues/123",
        "https://github.com/owner/repo/issues/abc",
    ];

    for input in cases {
        assert!(
            parse_issue_ref(input).is_err(),
            "Expected error for: {input}"
        );
    }
}

#[test]
fn test_offline_snapshot_byte_identical() {
    let fixture_path = PathBuf::from("tests/fixtures/issue-snapshot.json");

    // Resolve once
    let (task_a, prov_a) = resolve_issue_task(None, Some(fixture_path.clone())).unwrap();
    // Resolve again to verify byte-identical reproduction
    let (task_b, prov_b) = resolve_issue_task(None, Some(fixture_path)).unwrap();

    assert_eq!(task_a, task_b);
    assert_eq!(prov_a.issue_body_sha256, prov_b.issue_body_sha256);
    assert_eq!(prov_a.issue_repo, None);
    assert_eq!(prov_a.issue_number, None);

    // Verify PromptGuard envelopes exist in the generated task text and wrap the title
    assert!(task_a.contains(
        "<untrusted_task_text>\nFix the compiler panic on division by zero\n</untrusted_task_text>"
    ));
    assert!(task_a.contains("<untrusted_task_text>"));
    assert!(task_a.contains(
        "Running `cargo build` on a crate containing `/ 0` causes a compiler crash. Please fix it."
    ));
    assert!(task_a.contains("</untrusted_task_text>"));

    // Check comments are formatted
    assert!(task_a.contains("Discussion Comments:"));
    assert!(task_a.contains("octocat:"));
    assert!(task_a.contains("panic at main.rs:10"));
    assert!(task_a.contains("torvalds:"));
    assert!(task_a.contains("Actually, this might be related to constant evaluation."));
}

#[test]
fn test_comment_author_attribution() {
    let author_only = GithubIssueComment {
        body: Some("comment 1".to_string()),
        author: Some(GithubIssueCommentAuthor {
            login: "author_user".to_string(),
        }),
        user: None,
    };
    let user_only = GithubIssueComment {
        body: Some("comment 2".to_string()),
        author: None,
        user: Some(GithubIssueCommentAuthor {
            login: "user_user".to_string(),
        }),
    };
    let anonymous = GithubIssueComment {
        body: Some("comment 3".to_string()),
        author: None,
        user: None,
    };

    let prompt = format_issue_prompt("test", "test body", &[author_only, user_only, anonymous]);

    assert!(prompt.contains("author_user:\ncomment 1"));
    assert!(prompt.contains("user_user:\ncomment 2"));
    assert!(prompt.contains("anonymous:\ncomment 3"));
}

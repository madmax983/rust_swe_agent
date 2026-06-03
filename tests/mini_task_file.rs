#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs;
use std::process::Command;

fn run_mini(args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let out = Command::new(support::binary_path())
        .args(["--log", "error", "mini"])
        .args(args)
        .output()
        .expect("failed to spawn binary");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status, stdout, stderr)
}

fn run_mini_with_stdin(
    args: &[&str],
    stdin_content: &str,
) -> (std::process::ExitStatus, String, String) {
    use std::io::Write;
    let mut child = Command::new(support::binary_path())
        .args(["--log", "error", "mini"])
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn child");

    {
        let mut stdin = child.stdin.take().expect("failed to open stdin");
        stdin
            .write_all(stdin_content.as_bytes())
            .expect("failed to write stdin");
    }

    let out = child.wait_with_output().expect("failed to wait on child");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (out.status, stdout, stderr)
}

// ── AC 3: Mutual Exclusion ──────────────────────────────────────────────────

#[test]
fn both_specified_fails_mutual_exclusion() {
    let (status, _stdout, stderr) = run_mini(&[
        "--task",
        "hello",
        "--task-file",
        "tests/fixtures/render_only/snapshot.json",
        "--render-only",
        "--format",
        "json",
    ]);
    assert!(!status.success());
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains("multiple task sources provided"),
        "stderr must report mutual exclusion: {stderr}"
    );
    assert!(
        stderr.contains("outcome_class: usage_error"),
        "stderr must print stable outcome class usage_error: {stderr}"
    );
}

#[test]
fn neither_specified_fails() {
    let (status, _stdout, stderr) = run_mini(&["--render-only", "--format", "json"]);
    assert!(!status.success());
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains(
            "either --task, --task-file, --from-issue, or --from-issue-file must be provided"
        ),
        "stderr must report missing task argument: {stderr}"
    );
    assert!(
        stderr.contains("outcome_class: usage_error"),
        "stderr must print stable outcome class usage_error: {stderr}"
    );
}

// ── AC 4: Empty/Whitespace-only validation ──────────────────────────────────

#[test]
fn empty_task_fails() {
    let (status, _stdout, stderr) =
        run_mini(&["--task", "   \n  ", "--render-only", "--format", "json"]);
    assert!(!status.success());
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains("empty --task source"),
        "stderr must report empty task: {stderr}"
    );
}

#[test]
fn empty_task_file_fails() {
    let temp_dir = tempfile::tempdir().unwrap();
    let empty_file = temp_dir.path().join("empty.txt");
    fs::write(&empty_file, "   \n\t  ").unwrap();

    let (status, _stdout, stderr) = run_mini(&[
        "--task-file",
        empty_file.to_str().unwrap(),
        "--render-only",
        "--format",
        "json",
    ]);
    assert!(!status.success());
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains("empty task source from"),
        "stderr must report empty source: {stderr}"
    );
}

#[test]
fn empty_task_stdin_fails() {
    let (status, _stdout, stderr) = run_mini_with_stdin(
        &["--task-file", "-", "--render-only", "--format", "json"],
        "    \n   ",
    );
    assert!(!status.success());
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains("empty task source from `-`"),
        "stderr must report empty stdin: {stderr}"
    );
}

// ── AC 5: Nonexistent/Unreadable Task File ──────────────────────────────────

#[test]
fn nonexistent_task_file_fails() {
    let (status, _stdout, stderr) = run_mini(&[
        "--task-file",
        "nonexistent_file_path_xyz.txt",
        "--render-only",
        "--format",
        "json",
    ]);
    assert!(!status.success());
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains("does not exist"),
        "stderr must report nonexistent file: {stderr}"
    );
}

// ── AC 1 & 7: Task file reads content correctly and works under render-only ──

#[test]
fn task_file_reads_content_exactly() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file = temp_dir.path().join("task.txt");
    let original_prompt = "Hello! This is a multi-line task.\n\"Quotes\", `backticks` and $VARs.\n";
    fs::write(&file, original_prompt).unwrap();

    let (status, stdout, stderr) = run_mini(&[
        "--task-file",
        file.to_str().unwrap(),
        "--render-only",
        "--format",
        "json",
    ]);
    assert!(status.success(), "run failed: {stderr}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let user_msg = json["user_message"].as_str().unwrap();
    assert!(
        user_msg.contains(original_prompt),
        "Rendered user message does not contain exact task prompt.\nExpected: {original_prompt}\nGot: {user_msg}"
    );
}

// ── AC 2: Task file - reads stdin correctly ──────────────────────────────────

#[test]
fn task_file_stdin_reads_content_exactly() {
    let original_prompt = "Hello from stdin!\n\"Double quotes\" and `backticks` here too.";

    let (status, stdout, stderr) = run_mini_with_stdin(
        &["--task-file", "-", "--render-only", "--format", "json"],
        original_prompt,
    );
    assert!(status.success(), "run failed: {stderr}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let user_msg = json["user_message"].as_str().unwrap();
    assert!(
        user_msg.contains(original_prompt),
        "Rendered user message does not contain exact stdin task prompt.\nExpected: {original_prompt}\nGot: {user_msg}"
    );
}

// ── AC 6: BOM Stripping and byte-identity ────────────────────────────────────

#[test]
fn task_file_bom_stripped_but_rest_identical() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file = temp_dir.path().join("task_bom.txt");
    let mut file_content = Vec::new();
    file_content.extend_from_slice(&[0xEF, 0xBB, 0xBF]); // UTF-8 BOM
    let actual_text = "Prompt after BOM.\nLine 2.\n";
    file_content.extend_from_slice(actual_text.as_bytes());
    fs::write(&file, &file_content).unwrap();

    let (status, stdout, stderr) = run_mini(&[
        "--task-file",
        file.to_str().unwrap(),
        "--render-only",
        "--format",
        "json",
    ]);
    assert!(status.success(), "run failed: {stderr}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let user_msg = json["user_message"].as_str().unwrap();
    assert!(
        user_msg.contains(actual_text),
        "Rendered user message must contain exact text following the BOM.\nExpected: {actual_text}\nGot: {user_msg}"
    );
    assert!(
        !user_msg.contains('\u{FEFF}'),
        "BOM must be stripped from the task string"
    );
}

// ── AC 10: Success Metric (50-line markdown with escaping tests & SHA-256) ───

#[test]
fn fifty_line_task_sha256_verification() {
    let temp_dir = tempfile::tempdir().unwrap();
    let file = temp_dir.path().join("fifty_lines.md");

    // Construct exactly 50 lines of complex markdown with double quotes, backticks, $VAR, and Unicode
    let mut prompt = String::new();
    for i in 1..=50 {
        match i {
            5 => prompt.push_str("Line 5: An example of a \"double quoted\" string.\n"),
            10 => {
                prompt
                    .push_str("Line 10: Let's do some code fences:\n```rust\nfn main() {}\n```\n");
            }
            15 => prompt.push_str("Line 15: References to $VARs like $PATH, $CARGO_HOME.\n"),
            25 => prompt.push_str("Line 25: Unicode chars: 🦀 Rust is extremely cozy! ❄️\n"),
            30 => prompt.push_str("Line 30: `backticks` for inline code.\n"),
            i if i == 50 => {
                let _ = write!(prompt, "Line {i}: This is the final line.");
            }
            _ => {
                let _ = writeln!(prompt, "Line {i}: Just regular text line for filler.");
            }
        }
    }

    fs::write(&file, &prompt).unwrap();

    let (status, stdout, stderr) = run_mini(&[
        "--task-file",
        file.to_str().unwrap(),
        "--render-only",
        "--format",
        "json",
    ]);
    assert!(status.success(), "run failed: {stderr}");

    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    // We want to extract the task field from the formatted user_message or the trajectory's task.
    // Wait, let's verify that the parsed task is byte-identical to the prompt.
    // In our implementation, we'll verify this by extracting the task value or the parsed prompt from JSON.
    // Wait, let's check how the JSON output holds the task. In RenderOnly's JSON output:
    // "user_message": "Task: <untrusted_task_text>\n...\n</untrusted_task_text>\n"
    // Let's extract the text inside `<untrusted_task_text>` and `</untrusted_task_text>`.
    let user_msg = json["user_message"].as_str().unwrap();
    let start_tag = "Task: <untrusted_task_text>\n";
    let end_tag = "\n</untrusted_task_text>";

    let start_idx = user_msg.find(start_tag).expect("Could not find start tag") + start_tag.len();
    let end_idx = user_msg.find(end_tag).expect("Could not find end tag");
    let extracted_task = &user_msg[start_idx..end_idx];

    let mut hasher_expected = Sha256::new();
    hasher_expected.update(prompt.as_bytes());
    let hash_expected = hasher_expected.finalize();

    let mut hasher_actual = Sha256::new();
    hasher_actual.update(extracted_task.as_bytes());
    let hash_actual = hasher_actual.finalize();

    assert_eq!(
        hash_expected, hash_actual,
        "Extracted task text is not byte-identical! SHA-256 mismatch.\nExpected:\n{prompt}\n\nGot:\n{extracted_task}"
    );
}

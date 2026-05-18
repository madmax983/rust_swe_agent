//! Golden trajectory round-trip: serialize a minimal hand-crafted
//! `mini-swe-agent-1.1` trajectory and confirm key order + required
//! fields match what Python emits. Also confirms an unknown field in
//! `info` round-trips through the `other` catchall.

#![allow(clippy::unwrap_used)]

use maxwells_daemon::model::MessageExtra;
use maxwells_daemon::trajectory::{MessageRecord, Trajectory, TrajectoryInfo};

fn sample_python_shaped_trajectory() -> &'static str {
    r#"{
  "trajectory_format": "mini-swe-agent-1.1",
  "info": {
    "task": "create /tmp/hello.txt",
    "model_name": "claude-opus-4-7",
    "exit_reason": "submitted",
    "final_output": "done",
    "total_cost_usd": 0.0012,
    "steps": 2,
    "unknown_future_field": 42
  },
  "messages": [
    {"role": "system", "content": "You are ..."},
    {"role": "user", "content": "Task: create /tmp/hello.txt"},
    {
      "role": "assistant",
      "content": "```bash\necho hi > /tmp/hello.txt\n```",
      "extra": {"actions": ["echo hi > /tmp/hello.txt"], "cost": 0.0006}
    },
    {"role": "user", "content": "Exit code: 0\nOutput:\n"},
    {
      "role": "assistant",
      "content": "COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\n```\ndone\n```",
      "extra": {"actions": ["__SUBMIT__"], "cost": 0.0006}
    }
  ]
}"#
}

#[test]
fn golden_round_trip_preserves_format() {
    let text = sample_python_shaped_trajectory();
    let t: Trajectory = serde_json::from_str(text).unwrap();

    assert_eq!(t.trajectory_format, "mini-swe-agent-1.1");
    assert_eq!(t.info.task.as_deref(), Some("create /tmp/hello.txt"));
    assert_eq!(t.info.total_cost_usd, Some(0.0012));
    assert_eq!(t.messages.len(), 5);

    // Forward-compat catchall.
    assert_eq!(
        t.info.other.get("unknown_future_field"),
        Some(&serde_json::json!(42))
    );

    // Re-serialize; first three keys must be format, info, messages in order.
    let re = serde_json::to_string_pretty(&t).unwrap();
    let fp = re.find("\"trajectory_format\"").unwrap();
    let ip = re.find("\"info\"").unwrap();
    let mp = re.find("\"messages\"").unwrap();
    assert!(fp < ip && ip < mp);
}

#[test]
fn emits_content_even_when_extra_empty() {
    let t = Trajectory {
        trajectory_format: "mini-swe-agent-1.1".into(),
        info: TrajectoryInfo::default(),
        messages: vec![MessageRecord {
            role: "system".into(),
            content: "x".into(),
            extra: MessageExtra::default(),
        }],
    };
    let s = serde_json::to_string(&t).unwrap();
    // Extra was empty → must be omitted from the serialized form.
    assert!(!s.contains("\"extra\""), "unexpected empty extra in: {s}");
    assert!(s.contains("\"role\":\"system\""));
}

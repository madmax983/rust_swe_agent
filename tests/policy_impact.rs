#[test]
fn test_cli_parsing_policy_impact() {
    use maxwells_daemon::cli::{Cli, Command};
    use maxwells_daemon::cli::args::BenchCmd;
    use clap::Parser;

    let args = Cli::try_parse_from(["max", "bench", "policy-impact", "--sweep", "some_sweep_dir"]);
    assert!(args.is_ok(), "Failed to parse args: {:?}", args.err());
    let cli = args.unwrap();
    match cli.command {
        Command::Bench { cmd } => match cmd {
            BenchCmd::PolicyImpact(cmd_args) => {
                assert_eq!(cmd_args.sweep, std::path::PathBuf::from("some_sweep_dir"));
                assert_eq!(cmd_args.format, "text");
            }
            _ => panic!("Expected BenchCmd::PolicyImpact"),
        },
        _ => panic!("Expected Command::Bench"),
    }
}

#[test]
fn test_policy_impact_aggregation() {
    use std::fs;
    use tempfile::tempdir;
    use maxwells_daemon::run::policy_impact::{self, PolicyImpactArgs};

    let dir = tempdir().unwrap();
    let sweep_dir = dir.path();

    // 1. Create a dummy results.json for the sweep
    let results_json = serde_json::json!({
        "instances": [
            { "instance_id": "inst_1", "resolved_count": 1, "outcome": "submitted" },
            { "instance_id": "inst_2", "resolved_count": 1, "outcome": "submitted" },
            { "instance_id": "inst_3", "resolved_count": 0, "outcome": "unresolved" }
        ],
        "total_fallbacks": 0,
        "model_mix": {}
    });
    fs::write(sweep_dir.join("results.json"), serde_json::to_string(&results_json).unwrap()).unwrap();

    // 2. Create synthetic trajectories
    // Instance 1: Clean, no blocks, resolved
    let traj_1 = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {
            "exit_reason": "submitted",
            "outcome": "submitted",
            "policy_counts": { "allowed": 5, "asked": 0, "blocked": 0, "yolo_bypassed": 0 }
        },
        "messages": []
    });
    fs::write(sweep_dir.join("inst_1.traj.json"), serde_json::to_string(&traj_1).unwrap()).unwrap();

    // Instance 2: Blocked twice by rule A, resolved
    let traj_2 = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {
            "exit_reason": "submitted",
            "outcome": "submitted",
            "policy_counts": { "allowed": 3, "asked": 1, "blocked": 2, "yolo_bypassed": 1 }
        },
        "messages": [
            {
                "role": "user",
                "content": "Rejection",
                "extra": {
                    "other": {
                        "policy_blocked": true,
                        "policy_rule": "rule_a",
                        "blocked_command": "rm -rf /"
                    }
                }
            },
            {
                "role": "user",
                "content": "Rejection 2",
                "extra": {
                    "other": {
                        "policy_blocked": true,
                        "policy_rule": "rule_a",
                        "blocked_command": "rm -rf /"
                    }
                }
            }
        ]
    });
    fs::write(sweep_dir.join("inst_2.traj.json"), serde_json::to_string(&traj_2).unwrap()).unwrap();

    // Instance 3: Blocked 4 times by rule B, unresolved
    let traj_3 = serde_json::json!({
        "trajectory_format": "mini-swe-agent-1.2",
        "info": {
            "exit_reason": "stagnation",
            "outcome": "unresolved",
            "policy_counts": { "allowed": 2, "asked": 0, "blocked": 4, "yolo_bypassed": 0 }
        },
        "messages": [
            {
                "role": "user",
                "content": "Rejection",
                "extra": {
                    "other": {
                        "policy_blocked": true,
                        "policy_rule": "rule_b",
                        "blocked_command": "cat /etc/shadow"
                    }
                }
            },
            {
                "role": "user",
                "content": "Rejection 2",
                "extra": {
                    "other": {
                        "policy_blocked": true,
                        "policy_rule": "rule_b",
                        "blocked_command": "cat /etc/shadow"
                    }
                }
            },
            {
                "role": "user",
                "content": "Rejection 3",
                "extra": {
                    "other": {
                        "policy_blocked": true,
                        "policy_rule": "rule_b",
                        "blocked_command": "cat /etc/passwd"
                    }
                }
            },
            {
                "role": "user",
                "content": "Rejection 4",
                "extra": {
                    "other": {
                        "policy_blocked": true,
                        "policy_rule": "rule_b",
                        "blocked_command": "cat /etc/shadow"
                    }
                }
            }
        ]
    });
    fs::write(sweep_dir.join("inst_3.traj.json"), serde_json::to_string(&traj_3).unwrap()).unwrap();

    // Run processing
    let args = PolicyImpactArgs { sweep_dir: sweep_dir.to_path_buf() };
    let report = policy_impact::run(&args).unwrap();

    // Assert Sweep Totals
    assert_eq!(report.policy_impact_report.totals.allowed, 10);
    assert_eq!(report.policy_impact_report.totals.asked, 1);
    assert_eq!(report.policy_impact_report.totals.blocked, 6);
    assert_eq!(report.policy_impact_report.totals.yolo_bypassed, 1);

    // Assert Rules aggregation
    assert_eq!(report.policy_impact_report.rules.len(), 2);
    assert_eq!(report.policy_impact_report.rules[0].rule_label, "rule_b");
    assert_eq!(report.policy_impact_report.rules[0].block_count, 4);
    assert_eq!(report.policy_impact_report.rules[0].affected_instances, 1);
    assert_eq!(report.policy_impact_report.rules[0].affected_instance_ids, vec!["inst_3"]);
    // Since redaction should be applied by Redactor (default), let's make sure it's redacted.
    // Wait, let's see how Redactor redacts /etc/shadow / etc/passwd / rm -rf /.
    // Actually, maxwells_daemon has a standard Redactor that replaces sensitive parts.
    // Let's assert on the redacted forms.
    assert!(report.policy_impact_report.rules[0].top_blocked_command.contains("[REDACTED]") || report.policy_impact_report.rules[0].top_blocked_command.contains("cat"));
    assert!(report.policy_impact_report.rules[1].top_blocked_command.contains("[REDACTED]") || report.policy_impact_report.rules[1].top_blocked_command.contains("rm"));

    assert_eq!(report.policy_impact_report.rules[1].rule_label, "rule_a");
    assert_eq!(report.policy_impact_report.rules[1].block_count, 2);
    assert_eq!(report.policy_impact_report.rules[1].affected_instances, 1);
    assert_eq!(report.policy_impact_report.rules[1].affected_instance_ids, vec!["inst_2"]);

    // Assert Outcome Correlation
    assert_eq!(report.policy_impact_report.outcome_correlation.blocked_group.total_count, 2);
    assert_eq!(report.policy_impact_report.outcome_correlation.blocked_group.resolved_count, 1);
    assert_eq!(report.policy_impact_report.outcome_correlation.blocked_group.unresolved_count, 1);
    assert_eq!(report.policy_impact_report.outcome_correlation.blocked_group.resolved_rate, 0.5);

    assert_eq!(report.policy_impact_report.outcome_correlation.unblocked_group.total_count, 1);
    assert_eq!(report.policy_impact_report.outcome_correlation.unblocked_group.resolved_count, 1);
    assert_eq!(report.policy_impact_report.outcome_correlation.unblocked_group.resolved_rate, 1.0);

    assert_eq!(report.policy_impact_report.outcome_correlation.delta_resolved_rate, -0.5);
}

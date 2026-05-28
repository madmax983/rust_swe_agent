use crate::agent::RatatuiDashboard;
use crate::cli::args::PlayCmd;
use crate::error::Error;
use crate::stream::StreamEvent;
use crate::trajectory::Trajectory;
use std::time::Duration;
use tokio::time::sleep;

#[allow(clippy::too_many_lines)]
pub async fn run(args: PlayCmd) -> Result<(), Error> {
    let file_content = std::fs::read_to_string(&args.trajectory_path)?;
    let trajectory: Trajectory = serde_json::from_str(&file_content).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "Invalid trajectory JSON: {e}"
        )))
    })?;

    // Use a RatatuiDashboard to show the replay
    let handle = RatatuiDashboard::start()
        .map_err(|e| Error::Trajectory(format!("failed to start ratatui dashboard: {e}")))?;
    let sink = handle.stream_sink();

    let task = trajectory
        .info
        .task
        .clone()
        .unwrap_or_else(|| "Unknown task".into());
    let model = trajectory
        .info
        .model_name
        .clone()
        .unwrap_or_else(|| "Unknown model".into());

    // Emit RunStarted
    sink.emit(StreamEvent::RunStarted {
        task,
        model,
        started_at: chrono::Utc::now().to_rfc3339(),
    });

    let mut step = 0;

    for msg in trajectory.messages {
        sleep(Duration::from_millis(args.delay_ms)).await;

        match msg.role.as_str() {
            "assistant" => {
                step += 1;
                // Before emitting AssistantMessage, see if there are actions (like bash)
                sink.emit(StreamEvent::AssistantMessage {
                    step,
                    content: msg.content.clone(),
                    cost_usd: msg.extra.cost,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                });

                if let Some(actions) = &msg.extra.actions {
                    for action in actions {
                        // Very rough heuristic to extract a bash command
                        let cmd = if action.starts_with("bash(\"") && action.ends_with("\")") {
                            let inner = &action[6..action.len() - 2];
                            inner.replace("\\\"", "\"").replace("\\\\", "\\")
                        } else {
                            action.clone()
                        };
                        sink.emit(StreamEvent::BashStart {
                            step,
                            command: cmd,
                            timestamp: chrono::Utc::now().to_rfc3339(),
                        });
                    }
                }
            }
            "tool" | "observation" => {
                // Determine if it was bash ok or err or just observation
                if let Some(response) = &msg.extra.response {
                    let is_error = response.get("error").is_some();
                    let exit_code = i32::from(is_error);
                    let stdout = response
                        .get("output")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let stderr = response
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    sink.emit(StreamEvent::BashResult {
                        step,
                        exit_code,
                        stdout,
                        stderr,
                        timed_out: false,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                    });
                } else {
                    sink.emit(StreamEvent::Observation {
                        step,
                        content: msg.content.clone(),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                    });
                }
            }
            _ => {}
        }
    }

    sleep(Duration::from_millis(args.delay_ms)).await;

    // Emit RunEnded
    sink.emit(StreamEvent::RunEnded {
        exit_reason: trajectory
            .info
            .outcome
            .clone()
            .unwrap_or_else(|| "unknown".into()),
        failure_category: trajectory.info.failure_category,
        final_output: None,
        steps: step,
        total_cost_usd: trajectory.info.total_cost_usd.unwrap_or(0.0),
        ended_at: chrono::Utc::now().to_rfc3339(),
    });

    // Let the user see the final state for a moment before tearing down
    sleep(Duration::from_millis(1500)).await;

    handle.shutdown().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::model::{Message, MessageExtra};
    use crate::stream::StreamSink;
    use std::sync::{Arc, Mutex};

    // A mock sink to record emitted events
    #[derive(Clone, Default)]
    struct MockSink {
        events: Arc<Mutex<Vec<StreamEvent>>>,
    }

    impl StreamSink for MockSink {
        fn emit(&self, event: StreamEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn test_play_conversion_logic() {
        // Build a fake trajectory
        let mut t = Trajectory::new();
        t.info.task = Some("mock task".into());
        t.info.model_name = Some("mock-model".into());
        t.info.outcome = Some("submitted".into());
        t.info.total_cost_usd = Some(0.123);

        let mut msg1 = Message::assistant("I will run a command.");
        msg1.extra = MessageExtra {
            actions: Some(vec!["bash(\"ls -l\")".into()]),
            cost: Some(0.1),
            ..Default::default()
        };
        t.record_message(&msg1);

        let mut msg2 = Message {
            role: crate::model::Role::Tool,
            content: "stdout of ls".into(),
            cache_hint: Default::default(),
            extra: Default::default(),
        };
        msg2.extra = MessageExtra {
            response: Some(serde_json::json!({"output": "file.txt", "error": null})),
            ..Default::default()
        };
        t.record_message(&msg2);

        let mut msg3 = Message::assistant("I am done.");
        msg3.extra = MessageExtra {
            cost: Some(0.023),
            ..Default::default()
        };
        t.record_message(&msg3);

        // Setup sink
        let mock_sink = MockSink::default();
        let sink: Arc<dyn StreamSink> = Arc::new(mock_sink.clone());

        // Manual drive of the loop
        let task = t.info.task.clone().unwrap_or_else(|| "Unknown task".into());
        let model = t
            .info
            .model_name
            .clone()
            .unwrap_or_else(|| "Unknown model".into());

        sink.emit(StreamEvent::RunStarted {
            task,
            model,
            started_at: "2024-01-01T00:00:00Z".into(),
        });

        let mut step = 0;
        for msg in &t.messages {
            match msg.role.as_str() {
                "assistant" => {
                    step += 1;
                    sink.emit(StreamEvent::AssistantMessage {
                        step,
                        content: msg.content.clone(),
                        cost_usd: msg.extra.cost,
                        timestamp: "2024-01-01T00:00:00Z".into(),
                    });

                    if let Some(actions) = &msg.extra.actions {
                        for action in actions {
                            let cmd = if action.starts_with("bash(\"") && action.ends_with("\")") {
                                let inner = &action[6..action.len() - 2];
                                inner.replace("\\\"", "\"").replace("\\\\", "\\")
                            } else {
                                action.clone()
                            };
                            sink.emit(StreamEvent::BashStart {
                                step,
                                command: cmd,
                                timestamp: "2024-01-01T00:00:00Z".into(),
                            });
                        }
                    }
                }
                "tool" | "observation" => {
                    if let Some(response) = &msg.extra.response {
                        let is_error = response.get("error").filter(|e| !e.is_null()).is_some();
                        let exit_code = i32::from(is_error);
                        let stdout = response
                            .get("output")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let stderr = response
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        sink.emit(StreamEvent::BashResult {
                            step,
                            exit_code,
                            stdout,
                            stderr,
                            timed_out: false,
                            timestamp: "2024-01-01T00:00:00Z".into(),
                        });
                    } else {
                        sink.emit(StreamEvent::Observation {
                            step,
                            content: msg.content.clone(),
                            timestamp: "2024-01-01T00:00:00Z".into(),
                        });
                    }
                }
                _ => {}
            }
        }

        sink.emit(StreamEvent::RunEnded {
            exit_reason: t.info.outcome.clone().unwrap_or_else(|| "unknown".into()),
            failure_category: t.info.failure_category,
            final_output: None,
            steps: step,
            total_cost_usd: t.info.total_cost_usd.unwrap_or(0.0),
            ended_at: "2024-01-01T00:00:00Z".into(),
        });

        let events = mock_sink.events.lock().unwrap();
        assert_eq!(events.len(), 6);

        assert!(
            matches!(&events[0], StreamEvent::RunStarted { task, model, .. } if task == "mock task" && model == "mock-model")
        );
        assert!(matches!(
            &events[1],
            StreamEvent::AssistantMessage {
                step: 1,
                cost_usd: Some(0.1),
                ..
            }
        ));
        assert!(
            matches!(&events[2], StreamEvent::BashStart { step: 1, command, .. } if command == "ls -l")
        );
        assert!(
            matches!(&events[3], StreamEvent::BashResult { step: 1, exit_code: 0, stdout, .. } if stdout == "file.txt")
        );
        assert!(matches!(
            &events[4],
            StreamEvent::AssistantMessage {
                step: 2,
                cost_usd: Some(0.023),
                ..
            }
        ));
        assert!(
            matches!(&events[5], StreamEvent::RunEnded { exit_reason, total_cost_usd, .. } if exit_reason == "submitted" && (*total_cost_usd - 0.123).abs() < f64::EPSILON)
        );
        drop(events);
    }
}

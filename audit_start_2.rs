                    .get("exit_reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let outcome = inst
                    .get("outcome")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                instance_exit_reasons.insert(id.to_string(), exit_reason.to_string());
                results_outcomes.insert(id.to_string(), outcome.to_string());

                let mut expected_runs = inst
                    .get("runs")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1);
                if expected_runs == 0 {
                    expected_runs = 1;
                }
                instance_expected_runs.insert(id.to_string(), expected_runs);

                if exit_reason == "budget_halt"
                    || exit_reason == "budget_halted"
                    || outcome == "budget_halted"
                    || outcome == "skipped"
                    || exit_reason == "skipped"
                    || exit_reason == "skipped_resume"
                {
                    budget_halted_or_skipped_results.insert(id.to_string());
                }
            }
        }
    }

    let mut failed = false;
    let mut divergences = Vec::new();

    // Bijective checks: Trajectories vs Results
    for inst_id in &traj_instances {
        if !results_instances.contains(inst_id) {
            let msg = format!("audit:orphan:trajectory:{inst_id}");
            log_msg(&msg);
            divergences.push(msg);
            failed = true;
        }
    }

    for inst_id in &results_instances {
        if !traj_instances.contains(inst_id) {
            // Exempt if it was budget halted or skipped before starting
            if budget_halted_or_skipped_results.contains(inst_id) {
                continue;
            }
            let msg = format!("audit:missing:trajectory:{inst_id}");
            log_msg(&msg);
            divergences.push(msg);
            failed = true;
        }
    }

    // Bijective checks: Trajectories vs Evaluation (supporting legacy sb-cli format)
    let mut eval_instances = HashSet::new();
    let mut eval_resolved = HashMap::new();
    let mut is_legacy_format = false;
    if let Some(eval) = &evaluation {
        if let Some(instances) = eval.get("instances").and_then(serde_json::Value::as_array) {
            for inst in instances {
                if let Some(id) = inst.get("instance_id").and_then(serde_json::Value::as_str) {
                    eval_instances.insert(id.to_string());
                    if let Some(resolved) =
                        inst.get("resolved").and_then(serde_json::Value::as_bool)
                    {
                        eval_resolved.insert(id.to_string(), resolved);
                    }
                }
            }
        } else {
            // Support legacy sb-cli resolved_ids / submitted_ids structure
            is_legacy_format = true;
            if let Some(resolved_ids) = eval
                .get("resolved_ids")
                .and_then(serde_json::Value::as_array)
            {
                for id_val in resolved_ids {
                    if let Some(id) = id_val.as_str() {
                        eval_instances.insert(id.to_string());
                        eval_resolved.insert(id.to_string(), true);
                    }
                }
            }
            if let Some(submitted_ids) = eval
                .get("submitted_ids")
                .and_then(serde_json::Value::as_array)
            {
                for id_val in submitted_ids {
                    if let Some(id) = id_val.as_str() {
                        eval_instances.insert(id.to_string());
                        eval_resolved.entry(id.to_string()).or_insert(false);
                    }
                }
            }
        }

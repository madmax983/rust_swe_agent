If we use a trait we can define the `apply_config_overrides` function:
```rust
fn apply_config_overrides(cfg: &mut Config, cmd: &impl ConfigOverrides) -> Result<(), Error> {
    cfg.root.model.name = cmd.model().to_owned();
    cfg.root.agent.step_limit = cmd.step_limit();
    if let Some(v) = cmd.observation_max_bytes() {
        cfg.root.agent.observation_max_bytes = v;
    }
    if let Some(v) = cmd.observation_head_ratio() {
        validate_observation_head_ratio(v)?;
        cfg.root.agent.observation_head_ratio = v;
    }
    if let Some(kind) = cmd.env() {
        cfg.root.environment.kind = parse_env_kind(kind.as_str())?;
    }
    if let Some(img) = cmd.docker_image() {
        cfg.root.environment.docker_image = Some(img.clone());
    }
    if let Some(v) = cmd.per_task_budget_usd() {
        cfg.root.agent.per_task_budget_usd = Some(v);
    }
    if cmd.hide_budget_from_agent() {
        cfg.root.agent.hide_budget_from_agent = true;
    }
    if let Some(v) = cmd.detect_stagnation() {
        cfg.root.agent.detect_stagnation = v;
    }
    if let Some(v) = cmd.stagnation_repeat_threshold() {
        cfg.root.agent.stagnation_repeat_threshold = v;
    }
    if let Some(v) = cmd.stagnation_window() {
        cfg.root.agent.stagnation_window = v;
    }
    if let Some(v) = cmd.history_max_input_tokens() {
        cfg.root.agent.history_max_input_tokens = Some(v);
    }
    if let Some(v) = cmd.history_keep_last_observations() {
        cfg.root.agent.history_keep_last_observations = Some(v);
    }
    apply_mcp_server_overrides(cfg, cmd.mcp_servers())?;
    Ok(())
}
```

Wait, `clone_from` is better than `to_owned` since it can avoid allocating if the buffer is big enough, but here it's just setting from the struct initialization so `clone()` is fine or just `.into()`.

Let's do this directly. We will write a python script to revert the previous change and apply the trait.

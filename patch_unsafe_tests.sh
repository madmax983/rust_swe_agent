#!/bin/bash
set -e

# Replace unsafe { std::env::set_var } and remove_var with temp_env::with_var / temp_env::async_with_vars

# sweep_webhook.rs
sed -i 's/unsafe { std::env::set_var(&env_name, &unique_val) };/temp_env::async_with_vars([(\&env_name, Some(\&unique_val))], async || {/g' src/stream/sweep_webhook.rs
sed -i 's/unsafe { std::env::remove_var(&env_name) };/}).await;/g' src/stream/sweep_webhook.rs

# agent_doctor.rs
sed -i 's/unsafe { std::env::set_var(&var, "TOPSECRETVALUE") };/temp_env::with_var(\&var, Some("TOPSECRETVALUE"), || {/g' src/run/agent_doctor.rs
sed -i 's/unsafe { std::env::remove_var(&var) };/});/g' src/run/agent_doctor.rs

# mini.rs
sed -i 's/unsafe { std::env::set_var("TEST_MANIFEST_API_KEY", fake_secret) };/temp_env::async_with_vars([("TEST_MANIFEST_API_KEY", Some(fake_secret))], async || {/g' src/run/mini.rs
sed -i 's/unsafe { std::env::remove_var("TEST_MANIFEST_API_KEY") };/}).await;/g' src/run/mini.rs

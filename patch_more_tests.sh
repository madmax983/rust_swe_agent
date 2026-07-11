#!/bin/bash
set -e

# redact_audit.rs
sed -i 's/unsafe {/temp_env::with_var("DATABASE_PASSWORD", Some("super-secret-ci-token-value-123"), || {/g' src/run/redact_audit.rs
sed -i 's/std::env::set_var("DATABASE_PASSWORD", "super-secret-ci-token-value-123");/let report = audit(dir.path());/g' src/run/redact_audit.rs
sed -i 's/let report = audit(dir.path());/report/g' src/run/redact_audit.rs
sed -i 's/unsafe {//g' src/run/redact_audit.rs
sed -i 's/std::env::remove_var("DATABASE_PASSWORD");//g' src/run/redact_audit.rs
sed -i 's/        }//g' src/run/redact_audit.rs

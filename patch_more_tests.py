import re

with open('src/run/redact_audit.rs', 'r') as f:
    content = f.read()

content = re.sub(
    r'unsafe \{\s*std::env::set_var\("DATABASE_PASSWORD", "super-secret-ci-token-value-123"\);\s*\}\s*let report = audit\(dir\.path\(\)\);\s*unsafe \{\s*std::env::remove_var\("DATABASE_PASSWORD"\);\s*\}',
    r'''let report = temp_env::with_var("DATABASE_PASSWORD", Some("super-secret-ci-token-value-123"), || {
            audit(dir.path())
        });''',
    content
)

with open('src/run/redact_audit.rs', 'w') as f:
    f.write(content)

with open('src/telemetry/mod.rs', 'r') as f:
    content = f.read()

# Just use regex to replace all unsafe { ... } blocks in telemetry with temp_env::with_vars
# Wait, let's look at telemetry more carefully first.

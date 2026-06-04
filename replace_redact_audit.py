import re

with open("src/run/redact_audit.rs", "r") as f:
    content = f.read()

target = """        // SAFETY: set/remove a process-local var in a serial unit test.
        unsafe {
            std::env::set_var("DATABASE_PASSWORD", "super-secret-ci-token-value-123");
        }
        let report = audit(dir.path());
        unsafe {
            std::env::remove_var("DATABASE_PASSWORD");
        }"""

replacement = """        let report = temp_env::with_vars(
            [("DATABASE_PASSWORD", Some("super-secret-ci-token-value-123"))],
            || audit(dir.path()),
        );"""

if target in content:
    content = content.replace(target, replacement, 1)
    with open("src/run/redact_audit.rs", "w") as f:
        f.write(content)
    print("Replaced in src/run/redact_audit.rs")
else:
    print("Target not found")

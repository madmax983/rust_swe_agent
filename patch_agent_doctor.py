import re

with open('src/run/agent_doctor.rs', 'r') as f:
    content = f.read()

# credential_detail_never_contains_value
content = re.sub(
    r'unsafe \{ std::env::set_var\(&var, "TOPSECRETVALUE"\) \};\s*let check = check_credential\(model\);\s*unsafe \{ std::env::remove_var\(&var\) \};',
    r'let check = temp_env::with_var(&var, Some("TOPSECRETVALUE"), || check_credential(model));',
    content
)

# credential_fail_when_var_absent
content = re.sub(
    r'unsafe \{ std::env::remove_var\(&var\) \};\s*let check = check_credential\(model\);',
    r'let check = temp_env::with_var(&var, None::<&str>, || check_credential(model));',
    content
)

with open('src/run/agent_doctor.rs', 'w') as f:
    f.write(content)

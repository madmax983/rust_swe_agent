import sys

with open('src/trajectory/export.rs', 'r') as f:
    content = f.read()

content = content.replace('format!("**Task:** {}\\n\\n", task)', 'format!("**Task:** {task}\\n\\n")')
content = content.replace('format!("**Outcome:** {}\\n", outcome_redacted)', 'format!("**Outcome:** {outcome_redacted}\\n")')
content = content.replace('format!("### {}\\n\\n", role_title)', 'format!("### {role_title}\\n\\n")')
content = content.replace('format!("{}\\n", line)', 'format!("{line}\\n")')
content = content.replace('unwrap()', 'expect("Valid JSON")')

with open('src/trajectory/export.rs', 'w') as f:
    f.write(content)

with open('src/run/report.rs', 'r') as f:
    content = f.read()

# Fix clippy warnings
content = content.replace('format!("## {}\\n", block)', 'format!("## {block}\\n")')
content = content.replace('format!("{}\\n", line)', 'format!("{line}\\n")')

# Move md_to_jupyter function before tests
if 'fn md_to_jupyter' in content and '#[cfg(test)]\nmod tests' in content:
    tests_idx = content.find('#[cfg(test)]\nmod tests')
    jupyter_idx = content.find('\nfn md_to_jupyter')

    if jupyter_idx > tests_idx:
        jupyter_fn = content[jupyter_idx:]
        content = content[:jupyter_idx]

        content = content[:tests_idx] + jupyter_fn + '\n' + content[tests_idx:]

with open('src/run/report.rs', 'w') as f:
    f.write(content)

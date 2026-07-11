import re

# fix mini.rs
with open('src/run/mini.rs', 'r') as f:
    content = f.read()

content = re.sub(
    r'temp_env::async_with_vars\(\[("TEST_MANIFEST_API_KEY", Some\(fake_secret\))\]\, async \|\| \{',
    r'temp_env::async_with_vars([("TEST_MANIFEST_API_KEY", Some(fake_secret))], async {',
    content
)
with open('src/run/mini.rs', 'w') as f:
    f.write(content)

# fix sweep_webhook.rs
with open('src/stream/sweep_webhook.rs', 'r') as f:
    content = f.read()

content = re.sub(
    r'temp_env::async_with_vars\(\[\(\&env_name, Some\(\&unique_val\)\)\]\, async \|\| \{',
    r'temp_env::async_with_vars([(&env_name, Some(&unique_val))], async {',
    content
)
content = re.sub(
    r'let req = read_http\(socket\)\.await;\n\}\)\.await;\n        assert!\(\n            !req\.contains\(\&unique_val\),',
    r'let req = read_http(socket).await;\n            req\n        }).await;\n        assert!(\n            !req.contains(&unique_val),',
    content
)

with open('src/stream/sweep_webhook.rs', 'w') as f:
    f.write(content)

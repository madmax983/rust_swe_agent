import re
with open("Cargo.toml", "r") as f:
    content = f.read()
content = re.sub(r'mermaid-export = \[\]', 'mermaid-export = []\njupyter-export = []', content)
with open("Cargo.toml", "w") as f:
    f.write(content)

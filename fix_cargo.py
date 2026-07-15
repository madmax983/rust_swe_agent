import sys
content = open('Cargo.toml').read()
content = content.replace("yaml-export = []", "yaml-export = []") # This is fine actually because serde_yml is a core dependency "serde_yml = "0.0.12"" in the same file. It isn't optional
open('Cargo.toml', 'w').write(content)

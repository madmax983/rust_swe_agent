sed -i '/jupyter-export = \[\]/d' Cargo.toml
sed -i '/mermaid-export = \[\]/a jupyter-export = []' Cargo.toml

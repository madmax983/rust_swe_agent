with open("src/run/swebench.rs", "r") as f:
    content = f.read()

target = """                loop {
                    let n = match socket.read(&mut chunk).await {
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    if n == 0 {
                        break;
                    }"""

replacement = """                while let Ok(n) = socket.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }"""

if target in content:
    content = content.replace(target, replacement)
    with open("src/run/swebench.rs", "w") as f:
        f.write(content)
    print("Fixed swebench loop")
else:
    print("Not found")

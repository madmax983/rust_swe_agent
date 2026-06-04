with open("src/run/swebench.rs", "r") as f:
    content = f.read()

target = """                loop {
                    let n = match socket.read(&mut chunk).await {
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    // Very simple HTTP check to see if we reached end of headers
                    if buf.ends_with(b"\r\n\r\n") {
                        if let Ok(s) = String::from_utf8(buf.clone()) {
                            if let Some(body) = s.split("\r\n\r\n").nth(1) {
                                if !body.is_empty() {
                                    if let Ok(val) = serde_json::from_str(body) {
                                        collected_bg.lock().unwrap().push(val);
                                    }
                                }
                            }
                        }
                        break;
                    }
                }"""

replacement = """                while let Ok(n) = socket.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    // Very simple HTTP check to see if we reached end of headers
                    if buf.ends_with(b"\r\n\r\n") {
                        if let Ok(s) = String::from_utf8(buf.clone()) {
                            if let Some(body) = s.split("\r\n\r\n").nth(1) {
                                if !body.is_empty() {
                                    if let Ok(val) = serde_json::from_str(body) {
                                        collected_bg.lock().unwrap().push(val);
                                    }
                                }
                            }
                        }
                        break;
                    }
                }"""

if target in content:
    content = content.replace(target, replacement)
    with open("src/run/swebench.rs", "w") as f:
        f.write(content)
    print("Fixed swebench loop")

with open("src/stream/sweep_webhook.rs", "r") as f:
    content = f.read()

content = content.replace('Err(_e) => panic!("accept timed out"),', 'Err(e) => panic!("accept timed out: {e}"),')
content = content.replace('Err(_e) => panic!("read timed out"),', 'Err(e) => panic!("read timed out: {e}"),')

with open("src/stream/sweep_webhook.rs", "w") as f:
    f.write(content)
print("Fixed sweep_webhook Err(_e)")

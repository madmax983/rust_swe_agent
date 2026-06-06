#!/bin/bash
sed -i 's/req.split_once("\\r\\n\\r\\n").map(|(_, b)| b).unwrap_or("")/req.split_once("\\r\\n\\r\\n").map_or("", |(_, b)| b)/g' src/stream/sweep_webhook.rs
sed -i 's/Err(_) => panic!("accept timed out")/Err(tokio::time::error::Elapsed { .. }) => panic!("accept timed out")/g' src/stream/sweep_webhook.rs
sed -i 's/Err(_) => panic!("read timed out")/Err(tokio::time::error::Elapsed { .. }) => panic!("read timed out")/g' src/stream/sweep_webhook.rs

cat << 'INNER_EOF' | patch src/run/swebench.rs
--- src/run/swebench.rs
+++ src/run/swebench.rs
@@ -8024,11 +8024,7 @@
             let collected_clone = collected.clone();
             tokio::spawn(async move {
                 let mut chunk = [0u8; 8192];
-                loop {
-                    let n = match socket.read(&mut chunk).await {
-                        Ok(n) => n,
-                        Err(_) => break,
-                    };
+                while let Ok(n) = socket.read(&mut chunk).await {
                     if n == 0 {
                         break;
                     }
INNER_EOF

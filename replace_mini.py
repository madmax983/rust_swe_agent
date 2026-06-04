import re

with open("src/run/mini.rs", "r") as f:
    content = f.read()

target = """        let fake_secret = "sk-ant-fake-secret-value-for-test-0123456789abcdef";
        // SAFETY: test-only; single-threaded context for secret injection.
        unsafe { std::env::set_var("TEST_MANIFEST_API_KEY", fake_secret) };

        let args = make_mini_args_for_manifest_test(tmp.path().to_path_buf(), "secret-test");
        run(args).await.unwrap();

        // Clean up env var
        unsafe { std::env::remove_var("TEST_MANIFEST_API_KEY") };

        let traj_path = tmp.path().join("secret-test.traj.json");"""

replacement = """        let fake_secret = "sk-ant-fake-secret-value-for-test-0123456789abcdef";

        let traj_path = temp_env::async_with_vars(
            [("TEST_MANIFEST_API_KEY", Some(fake_secret))],
            Box::pin(async move {
                let args = make_mini_args_for_manifest_test(tmp.path().to_path_buf(), "secret-test");
                run(args).await.unwrap();
                tmp.path().join("secret-test.traj.json")
            })
        ).await;"""

if target in content:
    content = content.replace(target, replacement, 1)
    with open("src/run/mini.rs", "w") as f:
        f.write(content)
    print("Replaced in src/run/mini.rs")
else:
    print("Target not found in src/run/mini.rs")

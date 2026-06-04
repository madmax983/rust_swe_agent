import re

with open("src/run/mini.rs", "r") as f:
    content = f.read()

target = """        let traj_path = temp_env::async_with_vars(
            [("TEST_MANIFEST_API_KEY", Some(fake_secret))],
            Box::pin(async move {
                let args = make_mini_args_for_manifest_test(tmp.path().to_path_buf(), "secret-test");
                run(args).await.unwrap();
                tmp.path().join("secret-test.traj.json")
            })
        ).await;"""

# the issue is that tmp is moved into the future, and tempfile tempdirs are dropped
# when they go out of scope. If tmp is moved into the closure, it gets dropped
# when the closure finishes, before we can read the file!
# Wait, actually, tmp is moved into the async closure, so when the closure finishes, tmp is dropped!
# Then tmp.path().join() returns a path to a deleted directory.
# We need to just pass `tmp.path().to_path_buf()` into the closure and not move `tmp`.

replacement = """        #[allow(clippy::large_futures)]
        let traj_path = temp_env::async_with_vars(
            [("TEST_MANIFEST_API_KEY", Some(fake_secret))],
            Box::pin(async {
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
    print("Target not found")

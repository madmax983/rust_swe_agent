use std::path::PathBuf;

#[allow(dead_code)]
pub fn binary_path() -> PathBuf {
    cargo_bin_env_path().map_or_else(fallback_binary_path, PathBuf::from)
}

#[allow(dead_code)]
fn cargo_bin_env_path() -> Option<String> {
    std::env::var("CARGO_BIN_EXE_rust_swe_agent")
        .or_else(|_| std::env::var("CARGO_BIN_EXE_rust-swe-agent"))
        .ok()
}

#[allow(dead_code)]
fn fallback_binary_path() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    path.pop();
    path.push(format!("rust-swe-agent{}", std::env::consts::EXE_SUFFIX));
    path
}

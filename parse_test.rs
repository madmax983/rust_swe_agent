use std::path::{Path, PathBuf};

fn safe_instance_id(id: &str) -> Option<String> {
    if id.contains('/') || id.contains('\\') || id == ".." || id == "." || id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn main() {
    println!("{:?}", safe_instance_id("../../../../etc/passwd"));
    println!("{:?}", safe_instance_id("normal-id-123"));
    println!("{:?}", safe_instance_id("foo/bar"));
}

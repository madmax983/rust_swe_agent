fn safe_instance_id(id: &str) -> Result<String, String> {
    if id.contains('/') || id.contains('\\') || id == ".." || id == "." || id.is_empty() {
        Err(format!("invalid instance_id: '{}'", id))
    } else {
        Ok(id.to_string())
    }
}

fn main() {
    println!("{:?}", safe_instance_id("../../../../etc/passwd"));
}

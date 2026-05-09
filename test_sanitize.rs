fn safe_instance_id(id: &str) -> String {
    // Basic sanitization: remove all '/' and replace with '_'
    // Or just check if there's an issue and panic / return error.
    id.replace('/', "_").replace('\\', "_")
}
fn main() {
    println!("{}", safe_instance_id("../../../../etc/passwd"));
}

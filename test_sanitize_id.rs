fn sanitize_instance_id(id: &str) -> String {
    // If id is path traversal, we just keep alphanumeric or replace `/`
    id.replace('/', "_").replace('\\', "_")
}
fn main() {
    println!("{}", sanitize_instance_id("../../../../etc/passwd"));
}

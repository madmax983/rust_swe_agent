fn main() {
    let mut s = String::from("hello world 👋"); // 👋 is 4 bytes: F0 9F 91 8B
    if s.len() > 15 { // length is 16 bytes
        s.truncate(14); // truncates inside the emoji (fails)
    }
}

fn main() {
    let mut s = String::from("hello world 👋"); // 👋 is 4 bytes: F0 9F 91 8B
    if s.len() > 15 {
        let mut i = 14;
        while i > 0 && !s.is_char_boundary(i) {
            i -= 1;
        }
        s.truncate(i);
        s.push_str("...");
        println!("{}", s);
    }
}

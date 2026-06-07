#![warn(clippy::while_let_loop)]
fn read_dir() -> Result<i32, ()> { Ok(1) }
fn main() {
    loop {
        match read_dir() {
            Ok(n) => if n == 1 { break; },
            Err(_) => break,
        }
    }
}

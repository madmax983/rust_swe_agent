#![warn(clippy::while_let_loop)]
fn read_dir() -> Option<i32> { Some(1) }
fn main() {
    loop {
        if let Some(x) = read_dir() {
            println!("{}", x);
        } else {
            break;
        }
    }
}

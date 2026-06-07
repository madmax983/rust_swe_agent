#![warn(clippy::while_let_loop)]
fn main() {
    let mut i = 0;
    loop {
        match Some(i) {
            Some(n) => {
                if n == 5 { break; }
                i += 1;
            }
            None => break,
        }
    }
}

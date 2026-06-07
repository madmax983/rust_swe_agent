#![warn(clippy::while_let_loop)]
fn main() {
    let mut i = 0;
    loop {
        let Some(n) = Some(i) else { break; };
        if n == 5 { break; }
        i += 1;
    }
}

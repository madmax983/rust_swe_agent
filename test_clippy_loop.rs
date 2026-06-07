#![warn(clippy::while_let_loop)]
fn test() {
    let mut i = 0;
    loop {
        let Ok(n) = Result::<i32, ()>::Ok(i) else { break; };
        if n == 5 { break; }
        i += 1;
    }
}
fn main() { test(); }

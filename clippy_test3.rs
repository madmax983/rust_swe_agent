fn read_dir() -> Result<i32, ()> { Ok(1) }
fn main() {
    loop {
        let Ok(n) = read_dir() else { break; };
        if n == 1 { break; }
    }
}

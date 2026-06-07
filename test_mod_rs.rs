use std::path::Path;
fn test() {
    let output_dir = Path::new(".");
    let base_name = "test";
    let mut n = 2u32;
    let suffixed = loop {
        let suffixed = format!("{base_name}-{n}");
        if !output_dir.join(format!("{suffixed}.traj.json")).exists() {
            break suffixed;
        }
        n += 1;
    };
    println!("{}", suffixed);
}
fn main() { test(); }

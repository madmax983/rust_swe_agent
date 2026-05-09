use std::path::PathBuf;
fn legacy_trajectory_path_for(output_dir: &std::path::Path, instance_id: &str) -> PathBuf {
    output_dir.join(format!("{instance_id}.traj.json"))
}
fn trajectory_path_for_run(output_dir: &std::path::Path, instance_id: &str, run_index: u32) -> PathBuf {
    output_dir.join(instance_id).join(format!("run-{run_index}.traj.json"))
}
fn main() {
    let base = std::path::Path::new("/tmp/out");
    println!("{:?}", legacy_trajectory_path_for(base, "../../etc/passwd"));
    println!("{:?}", trajectory_path_for_run(base, "../../etc/passwd", 1));
}

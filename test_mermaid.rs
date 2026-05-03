#[cfg(feature = "mermaid-export")]
use rust_swe_agent::trajectory::export::MermaidExporter;
use rust_swe_agent::trajectory::Trajectory;
use rust_swe_agent::model::Message;
use rust_swe_agent::trajectory::export::TrajectoryExporter;

fn main() {
    let mut t = Trajectory::new();
    t.record_message(&Message::system("System prompt; echo 1 >&2"));
    let mermaid = MermaidExporter::export(&t);
    println!("{}", mermaid);
}

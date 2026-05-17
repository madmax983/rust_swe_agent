#[cfg(feature = "mermaid-export")]
use maxwells_daemon::trajectory::export::MermaidExporter;
use maxwells_daemon::trajectory::Trajectory;
use maxwells_daemon::model::Message;
use maxwells_daemon::trajectory::export::TrajectoryExporter;

fn main() {
    let mut t = Trajectory::new();
    t.record_message(&Message::system("System prompt; echo 1 >&2"));
    let mermaid = MermaidExporter::export(&t);
    println!("{}", mermaid);
}

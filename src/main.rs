//! Binary entrypoint for the `maxwells-daemon` agent.
//!
//! This crate provides the CLI interface to run the agent interactively or in
//! sweep mode across datasets.

use maxwells_daemon::exit_code::ExitCode;

#[tokio::main]
async fn main() {
    match Box::pin(maxwells_daemon::cli::run()).await {
        Ok(()) => {}
        Err(e) => {
            let code = ExitCode::from_error(&e);
            eprintln!("outcome_class: {}", code.outcome_class());
            eprintln!("error: {e}");
            std::process::exit(code.as_i32());
        }
    }
}

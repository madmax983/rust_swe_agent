//! The main entry point for the Maxwell's Daemon binary.
//! This provides the CLI interface.

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

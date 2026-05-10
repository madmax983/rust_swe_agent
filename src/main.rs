use rust_swe_agent::exit_code::ExitCode;

#[tokio::main]
async fn main() {
    match Box::pin(rust_swe_agent::cli::run()).await {
        Ok(()) => {}
        Err(e) => {
            let code = ExitCode::from_error(&e);
            eprintln!("outcome_class: {}", code.outcome_class());
            eprintln!("error: {e}");
            std::process::exit(code.as_i32());
        }
    }
}

use rust_swe_agent::cli;

#[tokio::main]
async fn main() -> Result<(), rust_swe_agent::Error> {
    cli::run().await
}

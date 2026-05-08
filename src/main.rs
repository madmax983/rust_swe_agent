use rust_swe_agent::cli;

#[tokio::main]
async fn main() -> Result<(), rust_swe_agent::Error> {
    Box::pin(cli::run()).await
}

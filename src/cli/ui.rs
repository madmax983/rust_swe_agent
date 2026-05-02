use clap::Args;
use std::path::PathBuf;

use crate::error::Error;

#[derive(Debug, Args)]
pub struct UiCmd {
    /// Sweep output directory produced by `bench swebench`
    #[arg(long)]
    pub sweep: PathBuf,

    /// Port to run the server on
    #[arg(long, default_value_t = 8080)]
    pub port: u16,
}

pub async fn run(cmd: UiCmd) -> Result<(), Error> {
    crate::web_ui::server::start_server(cmd.sweep, cmd.port).await
}

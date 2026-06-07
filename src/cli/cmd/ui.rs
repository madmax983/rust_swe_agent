use crate::cli::args;

use crate::error::Error;

#[cfg(feature = "ui-server")]
pub async fn ui_cmd(u: args::UiCmd) -> Result<(), Error> {
    #[cfg(not(feature = "ui-server"))]
    use crate::cli::cmd::util::exit_with_outcome;
    #[cfg(not(feature = "ui-server"))]
    use crate::exit_code::ExitCode;
    crate::run::ui::run(crate::run::ui::UiArgs {
        sweep: u.sweep,
        port: u.port,
        bind: u.bind,
        open: u.open,
    })
    .await
}

#[cfg(not(feature = "ui-server"))]
#[allow(clippy::unused_async)]
pub async fn ui_cmd(_u: args::UiCmd) -> Result<(), Error> {
    #[cfg(not(feature = "ui-server"))]
    use crate::cli::cmd::util::exit_with_outcome;
    #[cfg(not(feature = "ui-server"))]
    use crate::exit_code::ExitCode;
    exit_with_outcome(
        ExitCode::FeatureUnavailable,
        "the `ui` command requires the `ui-server` Cargo feature, which was not compiled in. \
         Rebuild with `cargo build --features ui-server`. \
         See docs/spec-web-ui.md for details.",
    );
}

#[cfg(feature = "docker")]
pub async fn cleanup_cmd() -> Result<(), Error> {
    let reaped = crate::env::docker::cleanup_orphans().await?;
    tracing::info!(count = reaped.len(), "reaped orphan containers");
    for id in reaped {
        println!("{id}");
    }
    Ok(())
}

#[cfg(not(feature = "docker"))]
pub fn cleanup_cmd() -> Result<(), Error> {
    Err(Error::Config(crate::error::ConfigError::Invalid(
        "docker feature not compiled in".into(),
    )))
}

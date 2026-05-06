//! Error types. One top-level `Error` composed of focused subsystem enums,
//! so call sites can match on the specific failure mode without a catch-all.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Model(#[from] ModelError),

    #[error(transparent)]
    Env(#[from] EnvError),

    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error("template render failed: {0}")]
    Template(String),

    #[error("trajectory io: {0}")]
    Trajectory(String),

    #[error("github pr: {0}")]
    Github(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model request failed: {0}")]
    Request(String),

    #[error("model returned malformed response: {0}")]
    Malformed(String),

    #[error("model refused request: {0}")]
    Refused(String),

    #[error("missing credentials: {0}")]
    MissingCredentials(String),

    #[error("rate limited: {0}")]
    RateLimited(String),
}

#[derive(Debug, Error)]
pub enum EnvError {
    #[error("command failed: {0}")]
    CommandFailed(String),

    #[error("command timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("docker not installed or not on PATH")]
    DockerNotInstalled,

    #[error("docker daemon unreachable: {0}")]
    DockerDaemonUnreachable(String),

    #[error("container start failed: {0}")]
    ContainerStartFailed(String),

    #[error("unexpected process exit: {0}")]
    UnexpectedExit(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(String),

    #[error("toml parse failed: {0}")]
    Toml(String),

    #[error("include chain exceeded {0} levels (possible cycle)")]
    IncludeDepthExceeded(usize),

    #[error("invalid config: {0}")]
    Invalid(String),
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        Self::Toml(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_error_from_toml_error() {
        // We simulate a toml deserialization error to test the From trait implementation
        let Err(toml_err) = toml::from_str::<serde::de::IgnoredAny>("invalid toml = {") else {
            panic!("expected error")
        };
        let config_err: ConfigError = toml_err.into();

        match config_err {
            ConfigError::Toml(msg) => {
                assert!(msg.contains("invalid toml"));
            }
            _ => panic!("Expected ConfigError::Toml"),
        }
    }

    #[test]
    fn test_error_display_implementations() {
        // Test that thiserror attributes render expected messages
        let err = ConfigError::NotFound("missing.toml".to_string());
        assert_eq!(err.to_string(), "config file not found: missing.toml");

        let err = EnvError::DockerNotInstalled;
        assert_eq!(err.to_string(), "docker not installed or not on PATH");

        let err = ModelError::RateLimited("too fast".to_string());
        assert_eq!(err.to_string(), "rate limited: too fast");

        // Test top level Error delegates appropriately
        let top_err: Error = err.into();
        assert_eq!(top_err.to_string(), "rate limited: too fast");

        let tmpl_err = Error::Template("bad syntax".to_string());
        assert_eq!(tmpl_err.to_string(), "template render failed: bad syntax");
    }
}

use axum::{
    Json, Router,
    extract::{Path, State},
    response::{Html, IntoResponse},
    routing::get,
};
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::Error;

pub struct WebUiState {
    pub sweep_dir: PathBuf,
}

pub async fn start_server(sweep_dir: PathBuf, port: u16) -> Result<(), Error> {
    let state = Arc::new(WebUiState { sweep_dir });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/api/instances", get(list_instances_handler))
        .route("/api/instances/:instance_id", get(get_instance_handler))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    tracing::info!("Starting Web UI server on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "Failed to bind to {addr}: {e}"
        )))
    })?;

    axum::serve(listener, app).await.map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "Server error: {e}"
        )))
    })?;

    Ok(())
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

async fn list_instances_handler(State(state): State<Arc<WebUiState>>) -> impl IntoResponse {
    let mut instances = vec![];

    // Scan directory for .traj.json files
    if let Ok(entries) = std::fs::read_dir(&state.sweep_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                    if file_name.ends_with(".traj.json") {
                        let instance_id = file_name.trim_end_matches(".traj.json").to_string();
                        instances.push(instance_id);
                    }
                }
            }
        }
    }

    instances.sort();
    Json(instances)
}

async fn get_instance_handler(
    State(state): State<Arc<WebUiState>>,
    Path(instance_id): Path<String>,
) -> impl IntoResponse {
    let args = crate::run::inspect::InspectArgs {
        sweep: state.sweep_dir.clone(),
        instance: Some(instance_id),
        filter: None,
        full: true,
    };

    match crate::run::inspect::run(&args) {
        Ok(out) => Json(serde_json::json!({
            "success": true,
            "data": out
        })),
        Err(e) => Json(serde_json::json!({
            "success": false,
            "error": e.to_string()
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn ui_state_can_be_created() {
        #[allow(clippy::unwrap_used)]
        let dir = tempdir().unwrap();
        let state = WebUiState {
            sweep_dir: dir.path().to_path_buf(),
        };
        assert_eq!(state.sweep_dir, dir.path().to_path_buf());
    }
}

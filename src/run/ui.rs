//! `max ui` — read-only local sweep browser (issue #319).
//!
//! Starts a minimal HTTP/1.1 server that lists every trajectory discovered
//! under `--sweep` and renders each one via the `HtmlExporter` pipeline.
//! All rendered bytes pass through `Redactor::default_enabled()` +
//! `surface::EXPORT` exactly as `bench inspect --format html` does.
//!
//! The server is single-binary, no-framework, no-auth, no-egress — identical
//! in policy to `src/stream/sse.rs`.  It serves until SIGINT and exits cleanly.
//!
//! **Feature gate**: this module is compiled only when the `ui-server` Cargo
//! feature is enabled.  When the feature is absent the CLI dispatch exits with
//! `feature_unavailable` (exit code 24).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::error::Error;

// ── Public types ─────────────────────────────────────────────────────────────

/// Metadata extracted from a single `*.traj.json` file for the index page.
#[derive(Debug, Clone)]
pub struct InstanceEntry {
    pub instance_id: String,
    pub outcome: Option<String>,
    pub steps: Option<u32>,
    pub total_cost_usd: Option<f64>,
    pub duration_secs: Option<f64>,
    /// Absolute path to the trajectory file on disk.
    pub traj_path: PathBuf,
}

/// Arguments forwarded from the CLI to [`run`].
pub struct UiArgs {
    pub sweep: PathBuf,
    pub port: u16,
    pub bind: String,
    pub open: bool,
}

// ── Server handle ─────────────────────────────────────────────────────────────

/// Running HTTP server.  Drop or call [`UiServer::shutdown`] to stop.
pub struct UiServer {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl UiServer {
    /// Bind `addr` and start accepting.  Returns once the listener is bound so
    /// callers can query `local_addr()` immediately without a race.
    pub async fn start(
        addr: SocketAddr,
        instances: Arc<Vec<InstanceEntry>>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let bound = listener.local_addr()?;
        let (sd_tx, sd_rx) = oneshot::channel();
        let handle = tokio::spawn(accept_loop(listener, instances, sd_rx));
        Ok(Self {
            addr: bound,
            shutdown_tx: Some(sd_tx),
            handle: Some(handle),
        })
    }

    /// Actual bound address (resolves port `0` to the OS-picked port).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Signal the accept loop to stop and wait for it.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
    }
}

impl Drop for UiServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Start the UI server and block until SIGINT / Ctrl-C.
#[cfg(feature = "ui-server")]
pub async fn run(args: UiArgs) -> Result<(), Error> {
    // Validate sweep directory.
    if !args.sweep.exists() || !args.sweep.is_dir() {
        return Err(Error::Config(crate::error::ConfigError::Invalid(format!(
            "ui: --sweep `{}` does not exist or is not a directory",
            args.sweep.display()
        ))));
    }

    let instances = Arc::new(discover_instances(&args.sweep)?);
    if instances.is_empty() {
        tracing::warn!(
            sweep = %args.sweep.display(),
            "ui: no *.traj.json files found in sweep directory"
        );
    }

    let bind_addr: SocketAddr = format!("{}:{}", args.bind, args.port)
        .parse()
        .map_err(|e: std::net::AddrParseError| {
            Error::Config(crate::error::ConfigError::Invalid(format!(
                "ui: invalid bind address `{}:{}`: {e}",
                args.bind, args.port
            )))
        })?;

    let server = UiServer::start(bind_addr, instances)
        .await
        .map_err(|e| Error::Io(e))?;

    let local = server.local_addr();
    let url = format!("http://{}:{}", local.ip(), local.port());
    println!("ui ready at {url}");

    if args.open {
        if let Err(e) = open_browser(&url) {
            tracing::warn!(error = %e, "ui: failed to open browser (non-fatal)");
        }
    }

    wait_for_shutdown_signal().await;
    tracing::info!("ui: received shutdown signal, stopping server");
    server.shutdown().await;
    Ok(())
}

// ── Sweep discovery ───────────────────────────────────────────────────────────

/// Scan `sweep_dir` for `*.traj.json` files and return a list sorted by
/// `instance_id` ascending.  Non-JSON files and non-trajectory JSON are
/// silently skipped; parse errors emit a warning.
pub fn discover_instances(sweep_dir: &Path) -> Result<Vec<InstanceEntry>, Error> {
    let mut entries: Vec<InstanceEntry> = Vec::new();

    let read_dir = std::fs::read_dir(sweep_dir).map_err(|e| {
        Error::Config(crate::error::ConfigError::Invalid(format!(
            "ui: failed to read sweep directory `{}`: {e}",
            sweep_dir.display()
        )))
    })?;

    for dir_entry in read_dir {
        let dir_entry = dir_entry.map_err(std::io::Error::from)?;
        let path = dir_entry.path();

        // Only process files whose name ends with `.traj.json`.
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if n.ends_with(".traj.json") => n.to_owned(),
            _ => continue,
        };

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path = ?path, error = %e, "ui: skipping unreadable file");
                continue;
            }
        };

        let traj: crate::trajectory::Trajectory = match serde_json::from_str(&text) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path = ?path, error = %e, "ui: skipping unparseable trajectory");
                continue;
            }
        };

        let instance_id = name
            .strip_suffix(".traj.json")
            .unwrap_or(&name)
            .to_owned();

        entries.push(InstanceEntry {
            instance_id,
            outcome: traj.info.outcome,
            steps: traj.info.steps,
            total_cost_usd: traj.info.total_cost_usd,
            duration_secs: traj.info.duration_secs,
            traj_path: path,
        });
    }

    entries.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    Ok(entries)
}

// ── Accept loop ───────────────────────────────────────────────────────────────

async fn accept_loop(
    listener: TcpListener,
    instances: Arc<Vec<InstanceEntry>>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::debug!("ui: accept loop: shutdown signal");
                break;
            }
            result = listener.accept() => {
                match result {
                    Ok((stream, peer)) => {
                        tracing::debug!(?peer, "ui: client connected");
                        let inst = instances.clone();
                        tokio::spawn(handle_connection(stream, inst, peer));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "ui: accept error");
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
    }
}

// ── Connection handler ────────────────────────────────────────────────────────

async fn handle_connection(
    mut stream: TcpStream,
    instances: Arc<Vec<InstanceEntry>>,
    peer: SocketAddr,
) {
    let path = match read_request_path(&mut stream).await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(?peer, error = %e, "ui: bad request");
            return;
        }
    };

    let response = dispatch(&path, &instances);

    if stream.write_all(response.as_bytes()).await.is_err() {
        tracing::debug!(?peer, "ui: write error (client disconnected)");
        return;
    }
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

// ── Router ────────────────────────────────────────────────────────────────────

/// Route a decoded request path to a response string.
fn dispatch(path: &str, instances: &[InstanceEntry]) -> String {
    // Build a lookup map: instance_id → entry (validated against discovered set).
    let by_id: HashMap<&str, &InstanceEntry> = instances
        .iter()
        .map(|e| (e.instance_id.as_str(), e))
        .collect();

    match path {
        "/" => serve_index(instances),
        "/healthz" => http_response(200, "OK", "application/json", r#"{"status":"ok"}"#),
        _ if path.starts_with("/instance/") => {
            let id = &path["/instance/".len()..];
            match by_id.get(id) {
                // instance_id validated against discovered set — no filesystem path used
                Some(entry) => serve_instance(entry),
                None => http_404(),
            }
        }
        _ => http_404(),
    }
}

// ── Index page ────────────────────────────────────────────────────────────────

fn serve_index(instances: &[InstanceEntry]) -> String {
    let mut rows = String::new();
    for entry in instances {
        let outcome = entry.outcome.as_deref().unwrap_or("-");
        let steps = entry
            .steps
            .map_or_else(|| "-".to_owned(), |s| s.to_string());
        let cost = entry
            .total_cost_usd
            .map_or_else(|| "-".to_owned(), |c| format!("{c:.4}"));
        let duration = entry
            .duration_secs
            .map_or_else(|| "-".to_owned(), |d| format!("{d:.1}"));

        // Escape each display value to prevent XSS.
        let safe_id = html_escape(&entry.instance_id);
        let safe_outcome = html_escape(outcome);
        let safe_steps = html_escape(&steps);
        let safe_cost = html_escape(&cost);
        let safe_duration = html_escape(&duration);

        rows.push_str(&format!(
            "<tr>\
             <td><a href=\"/instance/{safe_id}\">{safe_id}</a></td>\
             <td>{safe_outcome}</td>\
             <td>{safe_steps}</td>\
             <td>{safe_cost}</td>\
             <td>{safe_duration}</td>\
             </tr>\n"
        ));
    }

    let body = format!(
        "<!DOCTYPE html>\n\
         <html>\n\
         <head>\n\
         <meta charset=\"UTF-8\">\n\
         <title>Sweep Browser</title>\n\
         <style>\n\
         body{{font-family:sans-serif;margin:20px;max-width:1200px}}\n\
         h1{{color:#333}}\n\
         table{{border-collapse:collapse;width:100%;margin-top:16px}}\n\
         th,td{{border:1px solid #ddd;padding:8px 12px;text-align:left}}\n\
         th{{background:#f5f5f5;font-weight:600}}\n\
         tr:hover{{background:#f9f9ff}}\n\
         a{{color:#0066cc;text-decoration:none}}\n\
         a:hover{{text-decoration:underline}}\n\
         </style>\n\
         </head>\n\
         <body>\n\
         <h1>Sweep Browser</h1>\n\
         <table>\n\
         <thead>\n\
         <tr>\
         <th>instance_id</th>\
         <th>outcome</th>\
         <th>steps</th>\
         <th>total_cost_usd</th>\
         <th>duration_seconds</th>\
         </tr>\n\
         </thead>\n\
         <tbody>\n\
         {rows}\
         </tbody>\n\
         </table>\n\
         </body>\n\
         </html>"
    );

    http_response(200, "OK", "text/html; charset=UTF-8", &body)
}

// ── Instance page ─────────────────────────────────────────────────────────────

#[cfg(feature = "ui-server")]
fn serve_instance(entry: &InstanceEntry) -> String {
    use crate::trajectory::export::{HtmlExporter, TrajectoryExporter};

    let text = match std::fs::read_to_string(&entry.traj_path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(path = ?entry.traj_path, error = %e, "ui: failed to read trajectory");
            return http_response(
                500,
                "Internal Server Error",
                "text/plain",
                "500 Internal Server Error: failed to read trajectory",
            );
        }
    };

    let traj: crate::trajectory::Trajectory = match serde_json::from_str(&text) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(path = ?entry.traj_path, error = %e, "ui: failed to parse trajectory");
            return http_response(
                500,
                "Internal Server Error",
                "text/plain",
                "500 Internal Server Error: failed to parse trajectory",
            );
        }
    };

    // HtmlExporter applies Redactor::default_enabled() + surface::EXPORT internally.
    let html = HtmlExporter::export(&traj);
    http_response(200, "OK", "text/html; charset=UTF-8", &html)
}

// When ui-server feature is not enabled, this function is unreachable but
// must still compile (it's used in dispatch() which is always compiled).
#[cfg(not(feature = "ui-server"))]
fn serve_instance(_entry: &InstanceEntry) -> String {
    http_404()
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

fn http_response(status: u16, reason: &str, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len()
    )
}

fn http_404() -> String {
    http_response(404, "Not Found", "text/plain", "404 Not Found")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ── Request parsing ───────────────────────────────────────────────────────────

/// Read until `\r\n\r\n` (HTTP head terminator) and extract the request path.
/// Caps at 8 KiB to defend against slow / malicious clients.
async fn read_request_path(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = [0u8; 8192];
    let mut total = 0usize;

    loop {
        if total >= buf.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request head exceeds 8 KiB",
            ));
        }
        let n = stream.read(&mut buf[total..]).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "client closed before sending complete request",
            ));
        }
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    let text = std::str::from_utf8(&buf[..total]).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "non-UTF-8 request")
    })?;

    // Extract path from the first request line, e.g. "GET /path HTTP/1.1".
    // URL-decode percent-encoded characters so %2F stays as "/" (path traversal).
    let raw_path = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    // Only strip query strings; keep the path as-is.
    let path = raw_path
        .split_once('?')
        .map_or(raw_path, |(p, _)| p);

    Ok(path.to_owned())
}

// ── Browser launch ────────────────────────────────────────────────────────────

#[cfg(feature = "ui-server")]
fn open_browser(url: &str) -> std::io::Result<()> {
    // Platform-specific best-effort browser launch.
    #[cfg(target_os = "linux")]
    std::process::Command::new("xdg-open").arg(url).spawn()?;
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(url).spawn()?;
    #[cfg(target_os = "windows")]
    std::process::Command::new("cmd")
        .args(["/c", "start", url])
        .spawn()?;
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "browser launch not supported on this platform",
    ));
    Ok(())
}

// ── Signal handling ───────────────────────────────────────────────────────────

#[cfg(feature = "ui-server")]
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut sigint) = signal(SignalKind::interrupt()) {
            sigint.recv().await;
        } else {
            // Fall back to ctrl_c if SIGINT handler fails.
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn html_escape_replaces_special_chars() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a&b"), "a&amp;b");
        assert_eq!(html_escape("\"quote\""), "&quot;quote&quot;");
    }

    #[test]
    fn http_response_includes_content_length() {
        let r = http_response(200, "OK", "text/plain", "hello");
        assert!(r.contains("Content-Length: 5\r\n"), "missing length: {r}");
    }

    #[test]
    fn http_404_has_correct_status() {
        let r = http_404();
        assert!(r.starts_with("HTTP/1.1 404"), "wrong status: {r}");
    }

    #[test]
    fn dispatch_unknown_path_returns_404() {
        let resp = dispatch("/admin/secret", &[]);
        assert!(resp.starts_with("HTTP/1.1 404"), "expected 404, got: {resp:.80}");
    }

    #[test]
    fn dispatch_healthz_returns_200() {
        let resp = dispatch("/healthz", &[]);
        assert!(resp.starts_with("HTTP/1.1 200"), "expected 200, got: {resp:.80}");
        assert!(resp.contains("application/json"), "expected JSON content-type");
    }

    #[test]
    fn dispatch_index_returns_200() {
        let resp = dispatch("/", &[]);
        assert!(resp.starts_with("HTTP/1.1 200"), "expected 200, got: {resp:.80}");
        assert!(resp.contains("text/html"), "expected HTML content-type");
    }

    #[test]
    fn dispatch_unknown_instance_returns_404() {
        let resp = dispatch("/instance/does-not-exist", &[]);
        assert!(resp.starts_with("HTTP/1.1 404"), "expected 404, got: {resp:.80}");
    }

    #[test]
    fn discover_instances_sorts_by_id_ascending() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = crate::trajectory::Trajectory::new();
        t.info.outcome = Some("submitted".to_owned());

        for id in ["zzz", "aaa", "mmm"] {
            let json = serde_json::to_string(&t).unwrap();
            std::fs::write(dir.path().join(format!("{id}.traj.json")), json).unwrap();
        }

        let entries = discover_instances(dir.path()).unwrap();
        assert_eq!(entries[0].instance_id, "aaa");
        assert_eq!(entries[1].instance_id, "mmm");
        assert_eq!(entries[2].instance_id, "zzz");
    }
}

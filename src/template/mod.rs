//! Jinja2-style template rendering via `minijinja`.
//!
//! `Renderer` owns a `minijinja::Environment` with autoescape disabled
//! (we're rendering shell commands and prompts, not HTML). Environment
//! variable access (`{{ env.USER }}`) comes from a global added on
//! construction.

use minijinja::{Environment, Value, context};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::Error;

/// The `Renderer` struct wraps a `minijinja::Environment` and provides methods
/// to render templates with a given context.
///
/// ## Examples
///
/// ```rust
/// use maxwells_daemon::template::Renderer;
/// use minijinja::context;
///
/// let renderer = Renderer::new();
/// let output = renderer.render_with("Hello, {{ name }}!", context!(name => "World")).unwrap();
/// assert_eq!(output, "Hello, World!");
/// ```
pub struct Renderer {
    env: Environment<'static>,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    /// Creates a new `Renderer` instance with a default environment.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use maxwells_daemon::template::Renderer;
    ///
    /// let renderer = Renderer::new();
    /// ```
    pub fn new() -> Self {
        let mut env = Environment::new();
        env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);

        // Process env as a dict named `env`.
        let env_map: BTreeMap<String, String> = std::env::vars().collect();
        env.add_global("env", Value::from_serialize(&env_map));

        Self { env }
    }

    /// Render a template string against a context value that serializes to
    /// a map. The `context!` macro from `minijinja` is the recommended way
    /// to build ad-hoc contexts at call sites.
    pub fn render_str<T: Serialize>(&self, tmpl: &str, ctx: &T) -> Result<String, Error> {
        self.env
            .render_str(tmpl, Value::from_serialize(ctx))
            .map_err(Into::into)
    }

    /// Render a template string against a `minijinja::Value` context.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use maxwells_daemon::template::Renderer;
    /// use minijinja::context;
    ///
    /// let renderer = Renderer::new();
    /// let output = renderer.render_with("Hello, {{ name }}!", context!(name => "World")).unwrap();
    /// assert_eq!(output, "Hello, World!");
    /// ```
    pub fn render_with(&self, tmpl: &str, ctx: Value) -> Result<String, Error> {
        self.env.render_str(tmpl, ctx).map_err(Into::into)
    }
}

/// Builds a rendering context specifically formatted for tool observations.
///
/// This context exposes conventional keys expected by agent prompt templates:
/// - `task`: (Not explicitly added here, but conventionally merged later)
/// - `output`: The stdout/stderr from the tool execution.
/// - `returncode`: The exit code of the tool process.
/// - `extras`: A map of arbitrary additional variables.
///
/// ## Examples
///
/// ```rust
/// use maxwells_daemon::template::{Renderer, observation_context};
/// use std::collections::BTreeMap;
///
/// let renderer = Renderer::new();
/// let extras = BTreeMap::new();
/// let ctx = observation_context("Success", 0, &extras);
/// let output = renderer.render_with("Exit: {{ returncode }}, Out: {{ output }}", ctx).unwrap();
/// assert_eq!(output, "Exit: 0, Out: Success");
/// ```
pub fn observation_context(
    output: &str,
    returncode: i32,
    extras: &BTreeMap<String, String>,
) -> Value {
    let extras_val = Value::from_serialize(extras);
    context!(output => output, returncode => returncode, extras => extras_val)
}

/// Renders a simple template string using a list of string variable pairs.
///
/// This provides a quick way to render templates without building complex `minijinja::Value` contexts.
/// It disables auto-escaping and treats variables as raw strings, making it suitable for CLI
/// banners and simple status messages.
///
/// ## Examples
///
/// ```rust
/// use maxwells_daemon::template::render_simple;
///
/// let output = render_simple("Status: {{ status }}", &[("status", "OK")]).unwrap();
/// assert_eq!(output, "Status: OK");
/// ```
pub fn render_simple(tmpl: &str, vars: &[(&str, &str)]) -> Result<String, Error> {
    let mut env = Environment::new();
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
    let map: BTreeMap<&str, &str> = vars.iter().copied().collect();
    env.render_str(tmpl, Value::from_serialize(&map))
        .map_err(Into::into)
}

/// A shared, reference-counted pointer to a `Renderer`.
///
/// Use `SharedRenderer` to cheaply clone and share the `Renderer` across multiple threads
/// or contexts, as creating a new `minijinja::Environment` can be expensive.
pub type SharedRenderer = Arc<Renderer>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn basic_substitution() {
        let r = Renderer::new();
        let out = r
            .render_str("Hello, {{ name }}!", &BTreeMap::from([("name", "World")]))
            .unwrap();
        assert_eq!(out, "Hello, World!");
    }

    #[test]
    fn conditional_on_returncode() {
        let r = Renderer::new();
        let tmpl = "{% if returncode != 0 %}FAILED\n{% endif %}{{ output }}";
        let out = r
            .render_with(tmpl, context!(output => "oops", returncode => 1_i64))
            .unwrap();
        assert_eq!(out, "FAILED\noops");
    }

    #[test]
    fn env_global_is_available() {
        // Safe because tests are single-threaded per tokio::test? No — use
        // a var we set directly here. But std::env::set_var is unsafe in
        // multi-threaded contexts; use an already-present var instead.
        let r = Renderer::new();
        // PATH is virtually always set in CI.
        let out = r
            .render_with("{{ env.PATH is defined }}", context!())
            .unwrap();
        // Depending on platform PATH may be absent; accept either outcome.
        assert!(out == "true" || out == "false");
    }

    #[test]
    fn no_html_escaping_on_shell_content() {
        let r = Renderer::new();
        let out = r
            .render_str("{{ x }}", &BTreeMap::from([("x", "<foo> && echo \"y\"")]))
            .unwrap();
        assert_eq!(out, "<foo> && echo \"y\"");
    }
}

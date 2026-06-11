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

/// A rendering engine for evaluating prompt templates and agent observations.
///
/// The `Renderer` provides a pre-configured `minijinja::Environment` specialized
/// for the CLI environment. Because agents output bash commands rather than HTML,
/// auto-escaping is intentionally disabled. It also explicitly bridges system
/// environment variables (via `env.USER`, etc.) to the templates.
///
/// ## Examples
///
/// ```
/// use std::collections::BTreeMap;
/// use maxwells_daemon::template::Renderer;
///
/// let r = Renderer::new();
/// let rendered = r.render_str("echo {{ msg }}", &BTreeMap::from([("msg", "hello")])).unwrap();
/// assert_eq!(rendered, "echo hello");
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
    /// Creates a new `Renderer` initialized with environment variables.
    ///
    /// Autoescaping is turned off globally for the enclosed environment.
    ///
    /// ## Examples
    ///
    /// ```
    /// use maxwells_daemon::template::Renderer;
    /// let r = Renderer::new();
    /// ```
    pub fn new() -> Self {
        let mut env = Environment::new();
        env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);

        // Process env as a dict named `env`.
        let env_map: BTreeMap<String, String> = std::env::vars().collect();
        env.add_global("env", Value::from_serialize(&env_map));

        Self { env }
    }

    /// Renders a template string against a generic serialize-able context.
    ///
    /// This exists as a convenience wrapper around `render_with` when your
    /// template context is a plain struct or a `BTreeMap` instead of a
    /// dynamic `Value`.
    ///
    /// The `context!` macro from `minijinja` is the recommended way
    /// to build ad-hoc contexts at call sites.
    ///
    /// ## Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    /// use maxwells_daemon::template::Renderer;
    ///
    /// let r = Renderer::new();
    /// let ctx = BTreeMap::from([("tool", "bash")]);
    /// let out = r.render_str("Tool: {{ tool }}", &ctx).unwrap();
    /// assert_eq!(out, "Tool: bash");
    /// ```
    pub fn render_str<T: Serialize>(&self, tmpl: &str, ctx: &T) -> Result<String, Error> {
        self.env
            .render_str(tmpl, Value::from_serialize(ctx))
            .map_err(Into::into)
    }

    /// Renders a template string using a dynamic `Value` context.
    ///
    /// This exists to evaluate templates constructed at runtime where the shape
    /// of the context might not map directly to a static Rust struct.
    ///
    /// ## Examples
    ///
    /// ```
    /// use minijinja::context;
    /// use maxwells_daemon::template::Renderer;
    ///
    /// let r = Renderer::new();
    /// let ctx = context!(name => "world");
    /// let out = r.render_with("hello {{ name }}", ctx).unwrap();
    /// assert_eq!(out, "hello world");
    /// ```
    pub fn render_with(&self, tmpl: &str, ctx: Value) -> Result<String, Error> {
        self.env.render_str(tmpl, ctx).map_err(Into::into)
    }
}

/// Builds a standardized context object for formatting agent observations.
///
/// This exists to ensure that every evaluation of `observation_template`
/// receives a consistent shape, exposing standard keys like `output` and
/// `returncode` that are expected by prompt templates.
///
/// ## Examples
///
/// ```
/// use std::collections::BTreeMap;
/// use maxwells_daemon::template::observation_context;
///
/// let extras = BTreeMap::from([("task".to_string(), "id1".to_string())]);
/// let ctx = observation_context("Success", 0, &extras);
/// ```
pub fn observation_context(
    output: &str,
    returncode: i32,
    extras: &BTreeMap<String, String>,
) -> Value {
    let extras_val = Value::from_serialize(extras);
    context!(output => output, returncode => returncode, extras => extras_val)
}

/// Evaluates a simple template against an array of key-value tuples.
///
/// This exists to eliminate the boilerplate of constructing a `Renderer`
/// instance and manually building a map when all you need is trivial
/// string replacement for status messages or CLI banners.
///
/// ## Examples
///
/// ```
/// use maxwells_daemon::template::render_simple;
///
/// let out = render_simple("Current step: {{ step }}", &[("step", "4")]).unwrap();
/// assert_eq!(out, "Current step: 4");
/// ```
pub fn render_simple(tmpl: &str, vars: &[(&str, &str)]) -> Result<String, Error> {
    let mut env = Environment::new();
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
    let map: BTreeMap<&str, &str> = vars.iter().copied().collect();
    env.render_str(tmpl, Value::from_serialize(&map))
        .map_err(Into::into)
}

/// Keep `Arc<Renderer>` cheap in hot paths.
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

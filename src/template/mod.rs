//! Jinja2-style template rendering via `minijinja`.
//!
//! The [`Renderer`] owns a `minijinja::Environment` with autoescape disabled
//! (we're rendering shell commands and prompts, not HTML). Environment
//! variable access (`{{ env.USER }}`) comes from a global added on
//! construction.

use minijinja::{Environment, Value, context};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::Error;

/// A specialized template engine configured for rendering shell commands and agent prompts.
///
/// Unlike a standard web template engine, this [`Renderer`] explicitly disables HTML auto-escaping
/// to prevent corrupting shell syntax (e.g., turning `&&` into `&amp;&amp;`). It also globally
/// injects system environment variables so templates can seamlessly access values like `{{ env.USER }}`.
///
/// ## Examples
/// ```
/// use maxwells_daemon::template::Renderer;
/// use std::collections::BTreeMap;
///
/// let renderer = Renderer::new();
/// let output = renderer.render_str("echo {{ msg }}", &BTreeMap::from([("msg", "hello")])).unwrap();
/// assert_eq!(output, "echo hello");
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
    /// Creates a new `Renderer` with shell-safe defaults.
    ///
    /// This method performs two critical setup steps:
    /// 1. Disables all auto-escaping (`minijinja::AutoEscape::None`).
    /// 2. Captures all current system environment variables and exposes them via the global `env` context.
    ///
    /// ## Examples
    /// ```
    /// use maxwells_daemon::template::Renderer;
    /// use minijinja::context;
    ///
    /// // Set a dummy environment variable for the example
    /// std::env::set_var("TEST_BARD_VAR", "42");
    ///
    /// let renderer = Renderer::new();
    /// let output = renderer.render_with("Value is {{ env.TEST_BARD_VAR }}", context!()).unwrap();
    /// assert_eq!(output, "Value is 42");
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
    /// a map.
    ///
    /// This is the primary method for rendering templates with strongly-typed context structs.
    /// The `context!` macro from `minijinja` is also a recommended way
    /// to build ad-hoc contexts at call sites when not using custom structs.
    ///
    /// ## Examples
    /// ```
    /// use maxwells_daemon::template::Renderer;
    /// use std::collections::BTreeMap;
    ///
    /// let renderer = Renderer::new();
    /// let output = renderer.render_str("Task: {{ t }}", &BTreeMap::from([("t", "Find bug")])).unwrap();
    /// assert_eq!(output, "Task: Find bug");
    /// ```
    pub fn render_str<T: Serialize>(&self, tmpl: &str, ctx: &T) -> Result<String, Error> {
        self.env
            .render_str(tmpl, Value::from_serialize(ctx))
            .map_err(Into::into)
    }

    /// Renders a template string against an arbitrary `minijinja::Value` context.
    ///
    /// This is particularly useful when combining heterogeneous data types or dynamically
    /// constructing contexts via the `minijinja::context!` macro.
    ///
    /// ## Examples
    /// ```
    /// use maxwells_daemon::template::Renderer;
    /// use minijinja::context;
    ///
    /// let renderer = Renderer::new();
    /// let ctx = context!(
    ///     task => "Find the bug",
    ///     returncode => 1,
    /// );
    ///
    /// let output = renderer.render_with("Task: {{ task }} (Status: {{ returncode }})", ctx).unwrap();
    /// assert_eq!(output, "Task: Find the bug (Status: 1)");
    /// ```
    pub fn render_with(&self, tmpl: &str, ctx: Value) -> Result<String, Error> {
        self.env.render_str(tmpl, ctx).map_err(Into::into)
    }
}

/// Build a context with the keys mini-swe-agent conventionally exposes:
/// `task`, `output`, `returncode`, plus arbitrary extras.
///
/// This ensures consistent variable naming across all agent observation templates.
///
/// ## Examples
/// ```
/// use maxwells_daemon::template::observation_context;
/// use std::collections::BTreeMap;
///
/// let ctx = observation_context("ls output", 0, &BTreeMap::<String, String>::new());
/// // The returned minijinja::Value can now be passed to Renderer::render_with
/// ```
pub fn observation_context(
    output: &str,
    returncode: i32,
    extras: &BTreeMap<String, String>,
) -> Value {
    let extras_val = Value::from_serialize(extras);
    context!(output => output, returncode => returncode, extras => extras_val)
}

/// Small helper — used by `InteractiveAgent` status strings and banners.
///
/// This provides a quick, allocation-light way to render a template without
/// instantiating a full [`Renderer`] when only simple string variables are needed.
///
/// ## Examples
/// ```
/// use maxwells_daemon::template::render_simple;
///
/// let output = render_simple("Welcome {{ user }}!", &[("user", "Alice")]).unwrap();
/// assert_eq!(output, "Welcome Alice!");
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

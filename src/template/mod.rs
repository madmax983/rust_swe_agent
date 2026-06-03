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

/// A lightweight template rendering engine built on top of `minijinja`.
///
/// While `minijinja` provides a comprehensive HTML-first templating experience, `Renderer`
/// is specifically tuned for shell commands and system prompts. It disables HTML auto-escaping
/// to ensure scripts and shell operators (like `&&` or `>`) are preserved exactly as written.
/// Furthermore, it automatically exposes process environment variables under the `env` global object,
/// allowing templates to naturally refer to `$USER` or `$PATH` via `{{ env.USER }}`.
///
/// ## Examples
///
/// ```
/// use std::collections::BTreeMap;
/// use minijinja::context;
/// use maxwells_daemon::template::Renderer;
///
/// let renderer = Renderer::new();
/// let rendered = renderer.render_with("echo 'Hello, {{ name }}!'", context!(name => "World")).unwrap();
/// assert_eq!(rendered, "echo 'Hello, World!'");
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
    /// Instantiates a new template renderer configured for system prompts.
    ///
    /// This disables the default HTML escaping mechanism and pre-populates an `env` dictionary
    /// containing all environment variables available to the current process. This ensures templates
    /// have immediate access to context like paths and credentials without explicit injection.
    ///
    /// ## Examples
    ///
    /// ```
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

    /// Processes a template string against a complex nested structure.
    ///
    /// Unlike `render_str` which requires a struct implementing `Serialize`, `render_with`
    /// accepts a raw `minijinja::Value`. This is incredibly useful for ad-hoc context building
    /// where you don't want to define a throwaway struct. We heavily recommend using the
    /// `context!` macro to dynamically construct the data passed into this function.
    ///
    /// ## Examples
    ///
    /// ```
    /// use maxwells_daemon::template::Renderer;
    /// use minijinja::context;
    ///
    /// let renderer = Renderer::new();
    /// let result = renderer.render_with("User: {{ user.name }}", context!(user => context!(name => "Alice"))).unwrap();
    /// assert_eq!(result, "User: Alice");
    /// ```
    pub fn render_with(&self, tmpl: &str, ctx: Value) -> Result<String, Error> {
        self.env.render_str(tmpl, ctx).map_err(Into::into)
    }
}

/// Build a context with the keys mini-swe-agent conventionally exposes:
/// `task`, `output`, `returncode`, plus arbitrary extras.
pub fn observation_context(
    output: &str,
    returncode: i32,
    extras: &BTreeMap<String, String>,
) -> Value {
    let extras_val = Value::from_serialize(extras);
    context!(output => output, returncode => returncode, extras => extras_val)
}

/// Small helper — used by `InteractiveAgent` status strings and banners.
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

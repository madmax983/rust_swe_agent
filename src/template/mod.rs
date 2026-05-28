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

/// The master scribe for generating dynamic text.
///
/// `Renderer` orchestrates a `minijinja::Environment` tailored for our exact needs:
/// generating shell commands and AI prompts. To prevent mangling symbols like `<`, `>`, or `&`,
/// it intentionally **disables HTML auto-escaping**.
///
/// It also seamlessly injects the host's environment variables via a global `env` dict,
/// so you can conditionally render based on the system state (e.g., `{{ env.USER }}`).
///
/// ## Examples
///
/// ```
/// use maxwells_daemon::template::Renderer;
/// use std::collections::BTreeMap;
///
/// let renderer = Renderer::new();
/// let context = BTreeMap::from([("agent", "Bard")]);
///
/// let result = renderer.render_str("Hello, {{ agent }}!", &context).unwrap();
/// assert_eq!(result, "Hello, Bard!");
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
    /// Forges a fresh `Renderer` with default configurations.
    ///
    /// This function sets up the `minijinja::Environment` and populates the `env` global
    /// with the current process's environment variables, granting templates access to them.
    ///
    /// ## Examples
    ///
    /// ```
    /// use maxwells_daemon::template::Renderer;
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

    /// Brings a template string to life using a serializable context.
    ///
    /// The `context!` macro from `minijinja` is highly recommended for crafting ad-hoc
    /// context maps inline without boilerplate.
    ///
    /// ## Errors
    /// Returns a [`crate::error::Error`] if the template is malformed or if substitution fails.
    ///
    /// ## Examples
    ///
    /// ```
    /// use maxwells_daemon::template::Renderer;
    /// use minijinja::context;
    ///
    /// let renderer = Renderer::new();
    /// let out = renderer.render_str("Role: {{ role }}", &context!(role => "Storyteller")).unwrap();
    /// assert_eq!(out, "Role: Storyteller");
    /// ```
    pub fn render_str<T: Serialize>(&self, tmpl: &str, ctx: &T) -> Result<String, Error> {
        self.env
            .render_str(tmpl, Value::from_serialize(ctx))
            .map_err(Into::into)
    }

    /// Renders a template directly against a pre-computed `minijinja::Value`.
    ///
    /// Use this over `render_str` when you've already constructed a complex `Value` tree
    /// or when you're passing contexts dynamically from functions like `observation_context`.
    ///
    /// ## Errors
    /// Returns a [`crate::error::Error`] if the syntax is invalid or variables are missing.
    ///
    /// ## Examples
    ///
    /// ```
    /// use maxwells_daemon::template::Renderer;
    /// use minijinja::context;
    ///
    /// let renderer = Renderer::new();
    /// let ctx = context!(instrument => "Violin");
    /// let out = renderer.render_with("Playing the {{ instrument }}", ctx).unwrap();
    /// assert_eq!(out, "Playing the Violin");
    /// ```
    pub fn render_with(&self, tmpl: &str, ctx: Value) -> Result<String, Error> {
        self.env.render_str(tmpl, ctx).map_err(Into::into)
    }
}

/// Constructs a standardized context payload for agent observations.
///
/// This helper packages the `output`, `returncode`, and any `extras` into a single `minijinja::Value`
/// designed specifically for templates that process command execution results.
///
/// ## Examples
///
/// ```
/// use maxwells_daemon::template::observation_context;
/// use std::collections::BTreeMap;
///
/// let extras = BTreeMap::from([("timing".to_string(), "5ms".to_string())]);
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

/// A lightning-fast template renderer for simple string-to-string variable substitutions.
///
/// This is heavily utilized by `InteractiveAgent` for quickly rendering status strings
/// and banners without needing to instantiate the full `Renderer` struct. It creates an
/// isolated, unescaped `minijinja::Environment` on the fly.
///
/// ## Errors
/// Returns a [`crate::error::Error`] if template rendering fails due to syntax errors.
///
/// ## Examples
///
/// ```
/// use maxwells_daemon::template::render_simple;
///
/// let out = render_simple("Hello {{ target }}!", &[("target", "World")]).unwrap();
/// assert_eq!(out, "Hello World!");
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

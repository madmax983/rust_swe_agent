//! Core identifiers that maintain type boundaries across the system.
//!
//! In a complex agent environment, passing around raw strings and integers is a recipe
//! for subtle bugs. The ids module provides strongly-typed wrappers (Newtypes)
//! that ensure a `TaskId` cannot be accidentally swapped with a `ContainerId`, and
//! an index like `StepIdx` is always distinct from other numeric values.
//!
//! ## Examples
//!
//! ```
//! use rust_swe_agent::ids::{ContainerId, TaskId, StepIdx};
//!
//! let task = TaskId::new("issue-42");
//! let container = ContainerId::from("docker-abc");
//! let initial_step = StepIdx::zero();
//!
//! assert_eq!(task.as_str(), "issue-42");
//! assert_eq!(initial_step.get(), 0);
//! ```

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! string_id {
    ($name:ident, $doc:expr, $example:expr) => {
        #[doc = $doc]
        #[doc = ""]
        #[doc = "## Examples"]
        #[doc = ""]
        #[doc = "```"]
        #[doc = $example]
        #[doc = "```"]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Wraps an underlying string-like value into this strong type.
            ///
            /// This exists to cleanly convert from `&str` or `String` into the domain-specific ID.
            ///
            /// ## Examples
            ///
            /// ```
            /// use rust_swe_agent::ids::TaskId;
            /// let id = TaskId::new("my-task-123");
            /// ```
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            /// Exposes the inner string representation.
            ///
            /// Useful when interfacing with external systems (like Docker or logging)
            /// that require a primitive string reference.
            ///
            /// ## Examples
            ///
            /// ```
            /// use rust_swe_agent::ids::TaskId;
            /// let id = TaskId::new("my-task-123");
            /// assert_eq!(id.as_str(), "my-task-123");
            /// ```
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }
    };
}

string_id!(
    ContainerId,
    "A unique identifier for a sandboxed execution environment.",
    "use rust_swe_agent::ids::ContainerId;\nlet id = ContainerId::new(\"docker-container-xyz\");\nassert_eq!(id.as_str(), \"docker-container-xyz\");"
);

string_id!(
    TaskId,
    "A unique identifier for a specific problem instance or issue.",
    "use rust_swe_agent::ids::TaskId;\nlet id = TaskId::new(\"django-1234\");\nassert_eq!(id.as_str(), \"django-1234\");"
);

/// A zero-based index tracking the sequence of actions taken by an agent.
///
/// The `StepIdx` provides a safe, monotonically increasing counter used to align
/// trajectory recordings, state transitions, and cost tracking. It prevents
/// accidental arithmetic operations that might invalidate the sequential order.
///
/// ## Examples
///
/// ```
/// use rust_swe_agent::ids::StepIdx;
///
/// let first = StepIdx::zero();
/// let second = first.next();
/// assert_eq!(second.get(), 1);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StepIdx(pub u32);

impl StepIdx {
    /// Initializes a fresh step index starting at zero.
    ///
    /// This exists to establish the beginning of an agent's lifecycle securely.
    ///
    /// ## Examples
    ///
    /// ```
    /// use rust_swe_agent::ids::StepIdx;
    /// let start = StepIdx::zero();
    /// assert_eq!(start.get(), 0);
    /// ```
    pub const fn zero() -> Self {
        Self(0)
    }

    /// Advances the counter by one, capping at the maximum value of `u32`.
    ///
    /// The saturating addition ensures that a runaway loop does not cause a panic
    /// from integer overflow, allowing the system to halt gracefully.
    ///
    /// ## Examples
    ///
    /// ```
    /// use rust_swe_agent::ids::StepIdx;
    /// let current = StepIdx::zero();
    /// let advanced = current.next();
    /// assert_eq!(advanced.get(), 1);
    /// ```
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Retrieves the raw underlying integer value.
    ///
    /// Essential for formatting outputs, logging, or interacting with legacy systems
    /// that require raw numeric indexes.
    ///
    /// ## Examples
    ///
    /// ```
    /// use rust_swe_agent::ids::StepIdx;
    /// let step = StepIdx::zero();
    /// assert_eq!(step.get(), 0);
    /// ```
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for StepIdx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_id_creation_and_conversion() {
        let task_id = TaskId::new("task-123");
        assert_eq!(task_id.as_str(), "task-123");
        assert_eq!(task_id.to_string(), "task-123");

        let task_id_from_string = TaskId::from("task-123".to_string());
        assert_eq!(task_id, task_id_from_string);

        let task_id_from_str = TaskId::from("task-123");
        assert_eq!(task_id, task_id_from_str);

        let container_id = ContainerId::new("container-456");
        assert_eq!(container_id.as_str(), "container-456");
        assert_eq!(container_id.to_string(), "container-456");
    }

    #[test]
    fn test_step_idx() {
        let step = StepIdx::zero();
        assert_eq!(step.get(), 0);
        assert_eq!(step.to_string(), "0");

        let next_step = step.next();
        assert_eq!(next_step.get(), 1);
        assert_eq!(next_step.to_string(), "1");
    }
}

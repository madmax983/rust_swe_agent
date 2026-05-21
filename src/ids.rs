//! Strongly-typed identifiers to prevent class-of-string errors.
//!
//! Why use Newtypes? If we pass a `ContainerId` where a `TaskId` is expected,
//! the compiler catches it immediately. By wrapping `String` and `u32` in
//! specialized types, we increase domain clarity and reduce runtime bugs.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! string_id {
    (
        $(#[$meta:meta])*
        $name:ident
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            #[doc = concat!("Creates a new `", stringify!($name), "`.")]
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            #[doc = concat!("Returns a string slice for this `", stringify!($name), "`.")]
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
    /// A unique identifier for an isolated runtime container.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use maxwells_daemon::ids::ContainerId;
    ///
    /// let container = ContainerId::new("sandbox-x86-99");
    /// assert_eq!(container.as_str(), "sandbox-x86-99");
    /// ```
    ContainerId
);

string_id!(
    /// A unique identifier for a specific instance of a task.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// use maxwells_daemon::ids::TaskId;
    ///
    /// let task = TaskId::new("swe-bench-django-111");
    /// assert_eq!(task.as_str(), "swe-bench-django-111");
    /// ```
    TaskId
);

/// A monotonically increasing index representing the current step in an agent's trajectory.
///
/// ## Examples
///
/// ```rust
/// use maxwells_daemon::ids::StepIdx;
///
/// let start = StepIdx::zero();
/// assert_eq!(start.get(), 0);
///
/// let next = start.next();
/// assert_eq!(next.get(), 1);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StepIdx(pub u32);

impl StepIdx {
    /// Creates a new `StepIdx` starting at `0`.
    pub const fn zero() -> Self {
        Self(0)
    }

    /// Returns the next `StepIdx`, incrementing the internal counter by `1`.
    ///
    /// Uses saturating addition to prevent overflow panics.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Gets the raw `u32` value of this `StepIdx`.
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

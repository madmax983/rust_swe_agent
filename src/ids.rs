//! Newtype IDs. Keeps `TaskId` from being accidentally passed where a
//! `ContainerId` is expected.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
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

string_id!(ContainerId);
string_id!(TaskId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StepIdx(pub u32);

impl StepIdx {
    pub const fn zero() -> Self {
        Self(0)
    }
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
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

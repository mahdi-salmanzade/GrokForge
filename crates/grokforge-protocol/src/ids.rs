//! Newtyped identifiers used across the protocol. Each wraps a UUID so ids from
//! different domains can never be confused at a call site.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! id_newtype {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(Uuid);

        impl $name {
            /// Mint a fresh random id.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            /// The underlying UUID.
            #[must_use]
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // Prefix + short form keeps ids scannable in logs and the TUI.
                write!(f, "{}{}", $prefix, &self.0.simple().to_string()[..8])
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }
    };
}

id_newtype!(
    /// Identifies a conversation/session.
    SessionId, "sess_"
);
id_newtype!(
    /// Identifies a single submission from a frontend (codex "SQ" pattern).
    SubId, "sub_"
);
id_newtype!(
    /// Identifies one turn within a session.
    TurnId, "turn_"
);
id_newtype!(
    /// Identifies one tool call within a turn.
    ToolCallId, "call_"
);
id_newtype!(
    /// Identifies a pending approval request.
    ApprovalId, "appr_"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_typed() {
        let a = SessionId::new();
        let b = SessionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn display_is_prefixed_and_short() {
        let id = TurnId::new();
        let s = id.to_string();
        assert!(s.starts_with("turn_"));
        assert_eq!(s.len(), "turn_".len() + 8);
    }

    #[test]
    fn round_trips_through_json() {
        let id = ToolCallId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: ToolCallId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }
}

//! Newtyped identifiers used across the protocol. Locally minted ids wrap UUIDs so ids from
//! different domains can never be confused at a call site. Tool-call ids are opaque strings:
//! the exact provider-issued value must survive persistence and be replayed with tool results.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
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

            /// Parse the full UUID representation used by persistence.
            pub fn parse_str(value: &str) -> Result<Self, uuid::Error> {
                Uuid::parse_str(value).map(Self)
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

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse_str(value)
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
    /// Identifies a single submission from a frontend.
    SubId, "sub_"
);
id_newtype!(
    /// Identifies one turn within a session.
    TurnId, "turn_"
);
id_newtype!(
    /// Identifies a pending approval request.
    ApprovalId, "appr_"
);

/// Identifies one tool call within a turn.
///
/// Unlike GrokForge's locally owned ids, this value can originate at the model provider. It is
/// therefore deliberately opaque: changing or shortening it would make a subsequent
/// `function_call_output` refer to a call the provider does not know about.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolCallId(String);

impl ToolCallId {
    /// Mint a fresh id for a locally initiated tool-like operation.
    #[must_use]
    pub fn new() -> Self {
        Self(format!("call_{}", Uuid::new_v4().simple()))
    }

    /// Preserve an opaque tool-call id received from a provider or another protocol peer.
    #[must_use]
    pub fn from_raw(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The exact opaque id that must be used on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ToolCallId {
    fn default() -> Self {
        Self::new()
    }
}

impl From<Uuid> for ToolCallId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid.to_string())
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ToolCallId").field(&self.0).finish()
    }
}

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
        let id = ToolCallId::from_raw("provider_call_opaque-42");
        let json = serde_json::to_string(&id).unwrap();
        let back: ToolCallId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
        assert_eq!(back.to_string(), "provider_call_opaque-42");
    }

    #[test]
    fn tool_call_id_accepts_legacy_uuid_wire_values() {
        let legacy = "550e8400-e29b-41d4-a716-446655440000";
        let id: ToolCallId = serde_json::from_str(&format!(r#""{legacy}""#)).unwrap();
        assert_eq!(id.as_str(), legacy);
    }

    #[test]
    fn local_id_parses_full_persisted_uuid_without_uuid_api() {
        let raw = "550e8400-e29b-41d4-a716-446655440000";
        let id: SessionId = raw.parse().unwrap();
        assert_eq!(id.as_uuid().to_string(), raw);
        assert!("not-a-uuid".parse::<SessionId>().is_err());
    }
}

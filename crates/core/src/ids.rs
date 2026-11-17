//! Identifiers.

use std::fmt;
use std::sync::Arc;

/// Why a zone id was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The id is empty.
    #[error("zone id must not be empty")]
    Empty,
    /// The id contains a character outside `[a-z0-9_-]`.
    #[error("zone id {id:?} may only contain a-z, 0-9, '_' and '-'")]
    InvalidChar {
        /// The rejected id.
        id: String,
    },
}

/// A floor-heating zone id. Lower-case ASCII letters, digits, `_` and `-` only, because it
/// is embedded in MQTT topics (`energybrain/zone/<id>/...`). Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ZoneId(Arc<str>);

impl ZoneId {
    /// Validates and wraps an id.
    pub fn new(id: &str) -> Result<Self, IdError> {
        if id.is_empty() {
            return Err(IdError::Empty);
        }
        let valid = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if !id.chars().all(valid) {
            return Err(IdError::InvalidChar { id: id.to_owned() });
        }
        Ok(Self(Arc::from(id)))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl serde::Serialize for ZoneId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl fmt::Display for ZoneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_topic_safe_ids() {
        let id = ZoneId::new("bad_1-etg").unwrap();
        assert_eq!(id.as_str(), "bad_1-etg");
        assert_eq!(id.to_string(), "bad_1-etg");
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""bad_1-etg""#);
    }

    #[test]
    fn rejects_empty_and_unsafe_ids() {
        assert_eq!(ZoneId::new(""), Err(IdError::Empty));
        assert_eq!(IdError::Empty.to_string(), "zone id must not be empty");
        for bad in ["Bathroom", "a/b", "a+b", "a#", "bath room", "bå"] {
            let err = ZoneId::new(bad).unwrap_err();
            assert_eq!(err, IdError::InvalidChar { id: bad.to_owned() });
        }
        assert_eq!(
            ZoneId::new("a/b").unwrap_err().to_string(),
            r#"zone id "a/b" may only contain a-z, 0-9, '_' and '-'"#
        );
    }
}

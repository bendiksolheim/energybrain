//! Operating mode.

use std::fmt;

/// What the service is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    /// Guard disabled; nothing is computed for shedding and nothing is written.
    Off,
    /// Everything is computed and reported ("would write"), but no setpoint is written.
    #[default]
    Observe,
    /// Setpoints are written.
    Active,
}

impl Mode {
    /// The wire name (`off`, `observe`, `active`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Observe => "observe",
            Self::Active => "active",
        }
    }

    /// Parses a wire name, ignoring surrounding whitespace and case.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "observe" => Some(Self::Observe),
            "active" => Some(Self::Active),
            _ => None,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for m in [Mode::Off, Mode::Observe, Mode::Active] {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
            assert_eq!(m.to_string(), m.as_str());
        }
        assert_eq!(Mode::parse(" Active\n"), Some(Mode::Active));
        assert_eq!(Mode::parse("on"), None);
        assert_eq!(Mode::default(), Mode::Observe);
    }
}

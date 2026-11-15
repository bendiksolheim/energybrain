//! The energybrain service: runtime loop wiring core and adapters together.

/// Crate version, as reported at startup.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_the_package_version() {
        assert_eq!(super::version(), "0.1.0");
    }
}

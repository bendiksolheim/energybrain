//! Turns decisions into writes only when needed.
//!
//! A write happens when the target differs from the thermostat's current setpoint by more
//! than the deadband, and not more often than `min_interval` per zone — except restores,
//! which are never delayed (the guard's stagger already paces them). Nothing is written in
//! `Observe` (a "would write" is reported instead) or `Off`.

use jiff::{SignedDuration, Timestamp};

use crate::mode::Mode;
use crate::units::{Celsius, CelsiusDelta};
use crate::zone::Zone;

/// Actuator tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActuatorConfig {
    /// Differences up to this are not worth a write.
    pub deadband: CelsiusDelta,
    /// Minimum time between non-restore writes to one zone.
    pub min_interval: SignedDuration,
}

impl Default for ActuatorConfig {
    fn default() -> Self {
        Self {
            deadband: CelsiusDelta::new(0.25),
            min_interval: SignedDuration::from_secs(60),
        }
    }
}

/// What to do for one zone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Actuation {
    /// Nothing.
    None,
    /// Write this setpoint (active mode).
    Write(Celsius),
    /// Would write this setpoint (observe mode).
    WouldWrite(Celsius),
}

/// Decides whether to write `target` to `zone` at `now`. `restore` marks a write back
/// towards the base, which is never rate-limited.
pub fn actuate(
    cfg: &ActuatorConfig,
    mode: Mode,
    zone: &Zone,
    target: Celsius,
    now: Timestamp,
    restore: bool,
) -> Actuation {
    // In observe mode nothing reaches the thermostat, so our own would-writes stand in
    // for its state; otherwise trust what it reports.
    let (current, last) = match mode {
        Mode::Observe => (
            zone.last_would_write.map(|(v, _)| v).or(zone.reported),
            zone.last_would_write,
        ),
        Mode::Active | Mode::Off => (
            zone.reported.or(zone.last_write.map(|(v, _)| v)),
            zone.last_write,
        ),
    };
    let close = |a: Celsius, b: Celsius| (a - b).abs() <= cfg.deadband;
    if current.is_some_and(|c| close(c, target)) {
        return Actuation::None;
    }
    if let Some((value, at)) = last {
        let recent = now.duration_since(at) < cfg.min_interval;
        // Already sent and not yet echoed, or rate-limited.
        if recent && (close(value, target) || !restore) {
            return Actuation::None;
        }
    }
    match mode {
        Mode::Active => Actuation::Write(target),
        Mode::Observe => Actuation::WouldWrite(target),
        Mode::Off => Actuation::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zone::tests::{config, ts};

    fn c(v: f64) -> Celsius {
        Celsius::new(v)
    }

    fn zone(reported: Option<f64>) -> Zone {
        let mut z = Zone::new(config("bad", 1));
        z.reported = reported.map(c);
        z
    }

    const T0: &str = "2026-11-15T17:00:00Z";

    #[test]
    fn default_config() {
        let a = ActuatorConfig::default();
        assert_eq!(a.deadband, CelsiusDelta::new(0.25));
        assert_eq!(a.min_interval, SignedDuration::from_secs(60));
    }

    #[test]
    fn writes_only_outside_the_deadband() {
        let cfg = ActuatorConfig::default();
        let z = zone(Some(22.0));
        let now = ts(T0);
        assert_eq!(
            actuate(&cfg, Mode::Active, &z, c(22.2), now, false),
            Actuation::None
        );
        assert_eq!(
            actuate(&cfg, Mode::Active, &z, c(18.0), now, false),
            Actuation::Write(c(18.0))
        );
        // With nothing reported, write.
        assert_eq!(
            actuate(&cfg, Mode::Active, &zone(None), c(18.0), now, false),
            Actuation::Write(c(18.0))
        );
    }

    #[test]
    fn never_writes_in_observe_or_off() {
        let cfg = ActuatorConfig::default();
        let z = zone(Some(22.0));
        let now = ts(T0);
        assert_eq!(
            actuate(&cfg, Mode::Observe, &z, c(18.0), now, false),
            Actuation::WouldWrite(c(18.0))
        );
        assert_eq!(
            actuate(&cfg, Mode::Off, &z, c(18.0), now, true),
            Actuation::None
        );
    }

    #[test]
    fn rate_limit_applies_to_sheds_but_not_restores() {
        let cfg = ActuatorConfig::default();
        let mut z = zone(Some(18.0));
        z.last_write = Some((c(18.0), ts(T0)));
        let soon = ts(T0) + SignedDuration::from_secs(30);
        assert_eq!(
            actuate(&cfg, Mode::Active, &z, c(16.0), soon, false),
            Actuation::None
        );
        assert_eq!(
            actuate(&cfg, Mode::Active, &z, c(22.0), soon, true),
            Actuation::Write(c(22.0))
        );
        let later = ts(T0) + SignedDuration::from_secs(60);
        assert_eq!(
            actuate(&cfg, Mode::Active, &z, c(16.0), later, false),
            Actuation::Write(c(16.0))
        );
    }

    #[test]
    fn does_not_resend_while_waiting_for_the_echo() {
        let cfg = ActuatorConfig::default();
        let mut z = zone(Some(22.0));
        z.last_write = Some((c(18.0), ts(T0)));
        let soon = ts(T0) + SignedDuration::from_secs(10);
        assert_eq!(
            actuate(&cfg, Mode::Active, &z, c(18.0), soon, true),
            Actuation::None
        );
        // No echo after a minute: try again.
        let later = ts(T0) + SignedDuration::from_secs(61);
        assert_eq!(
            actuate(&cfg, Mode::Active, &z, c(18.0), later, true),
            Actuation::Write(c(18.0))
        );
    }

    #[test]
    fn observe_mode_tracks_its_own_would_writes() {
        let cfg = ActuatorConfig::default();
        let mut z = zone(Some(22.0));
        let now = ts(T0);
        z.last_would_write = Some((c(18.0), now));
        // The simulated thermostat is already at 18: nothing more to report.
        assert_eq!(
            actuate(&cfg, Mode::Observe, &z, c(18.0), now, false),
            Actuation::None
        );
        // Restoring is reported straight away.
        assert_eq!(
            actuate(&cfg, Mode::Observe, &z, c(22.0), now, true),
            Actuation::WouldWrite(c(22.0))
        );
    }
}

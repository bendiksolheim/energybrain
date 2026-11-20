//! The EV charger (Easee Charge Lite): what it reports and what we know about it.
//!
//! Readings come from polling Easee's cloud (PLAN.md §9), so they are tens of seconds old.
//! The HAN meter stays the truth for the hour's projection; a reading only tells the guard
//! what the car draws and what it is offered.

use jiff::{SignedDuration, Timestamp};

use crate::energy::Freshness;
use crate::units::{Ampere, Kw, Kwh, Volt};

/// The charger's operating mode (Easee observation 109).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChargerMode {
    /// The charger is offline.
    Offline,
    /// No car connected.
    Disconnected,
    /// A car is connected and the charger waits for the car or for load balancing.
    AwaitingStart,
    /// Charging.
    Charging,
    /// The car has paused or stopped charging.
    Completed,
    /// The charger is in an error state.
    Error,
    /// The charger offers current, but the car takes none.
    ReadyToCharge,
    /// Waiting for authentication.
    AwaitingAuthentication,
    /// De-authenticating.
    Deauthenticating,
    /// A code missing from Easee's table when this was written; treated as not charging.
    Unknown(u8),
}

impl ChargerMode {
    /// Maps Easee's op-mode code.
    pub const fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Offline,
            1 => Self::Disconnected,
            2 => Self::AwaitingStart,
            3 => Self::Charging,
            4 => Self::Completed,
            5 => Self::Error,
            6 => Self::ReadyToCharge,
            7 => Self::AwaitingAuthentication,
            8 => Self::Deauthenticating,
            other => Self::Unknown(other),
        }
    }

    /// Whether a car is plugged in. Only then can a limit change anything; unplugging also
    /// clears the charger's dynamic current by itself.
    pub const fn car_connected(self) -> bool {
        match self {
            Self::AwaitingStart
            | Self::Charging
            | Self::Completed
            | Self::ReadyToCharge
            | Self::AwaitingAuthentication => true,
            Self::Offline
            | Self::Disconnected
            | Self::Error
            | Self::Deauthenticating
            | Self::Unknown(_) => false,
        }
    }
}

/// One poll of the charger.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct ChargerReading {
    /// When it was polled (receive time, like meter power).
    pub at: Timestamp,
    /// Operating mode.
    pub mode: ChargerMode,
    /// What the car draws.
    pub power: Kw,
    /// Energy charged in the current session.
    #[serde(rename = "session_energy")]
    pub session: Kwh,
    /// The volatile limit (ours, or the app's "Temporary charging limit"), if one is set.
    pub dynamic_current: Option<Ampere>,
    /// The persistent limit (the app's "Charger limit"), if known.
    pub max_current: Option<Ampere>,
    /// What the pilot signal offers the car right now.
    pub offered: Ampere,
    /// Whether the charger is connected to Easee's cloud. When it isn't, the reading is
    /// cached and writes can't reach it.
    pub connected: bool,
}

/// Static configuration of the charger.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChargerConfig {
    /// Power per ampere of offered current: 230 V per phase the car charges on.
    pub voltage: Volt,
    /// With no reading for this long, the charger counts as stale.
    pub stale_after: SignedDuration,
}

impl Default for ChargerConfig {
    /// One phase on a 230 V IT net, stale after three missed 30 s polls and some slack.
    fn default() -> Self {
        Self {
            voltage: Volt::new(230.0),
            stale_after: SignedDuration::from_mins(3),
        }
    }
}

/// The charger and what is known about it at runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct Charger {
    /// Static configuration.
    pub config: ChargerConfig,
    /// The latest reading.
    pub reading: Option<ChargerReading>,
}

impl Charger {
    /// A charger nothing has been heard from yet.
    pub const fn new(config: ChargerConfig) -> Self {
        Self {
            config,
            reading: None,
        }
    }

    /// Records a reading. One older than the latest (a slow poll overtaken by a faster
    /// one) is ignored.
    pub fn on_reading(&mut self, reading: ChargerReading) {
        if self.reading.is_none_or(|r| reading.at >= r.at) {
            self.reading = Some(reading);
        }
    }

    /// How current the latest reading is.
    pub fn freshness(&self, now: Timestamp) -> Freshness {
        match self.reading {
            None => Freshness::NoData,
            Some(r) if now.duration_since(r.at) > self.config.stale_after => Freshness::Stale,
            Some(_) => Freshness::Fresh,
        }
    }

    /// Whether the guard can rely on the charger at `now`: a fresh reading from a charger
    /// that is connected to the cloud (so a write would reach it).
    pub fn available(&self, now: Timestamp) -> bool {
        self.freshness(now) == Freshness::Fresh
            && self
                .reading
                .is_some_and(|r| r.connected && r.mode != ChargerMode::Offline)
    }

    /// The power the car would draw if offered `current`.
    pub fn power_at(&self, current: Ampere) -> Kw {
        current * self.config.voltage
    }

    /// Status for publishing.
    pub fn status(&self, now: Timestamp) -> ChargerStatus {
        ChargerStatus {
            freshness: self.freshness(now),
            available: self.available(now),
            reading: self.reading,
        }
    }
}

/// The charger's part of the status.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct ChargerStatus {
    /// How current the latest reading is.
    pub freshness: Freshness,
    /// Whether the guard can rely on it (fresh and connected).
    pub available: bool,
    /// The latest reading.
    pub reading: Option<ChargerReading>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::zone::tests::ts;

    pub(crate) fn reading(at: &str, mode: ChargerMode, power: f64) -> ChargerReading {
        ChargerReading {
            at: ts(at),
            mode,
            power: Kw::new(power),
            session: Kwh::new(3.2),
            dynamic_current: None,
            max_current: Some(Ampere::new(15.0)),
            offered: Ampere::new(10.0),
            connected: true,
        }
    }

    #[test]
    fn maps_every_op_mode_code() {
        let modes: Vec<_> = (0..=9).map(ChargerMode::from_code).collect();
        assert_eq!(
            modes,
            [
                ChargerMode::Offline,
                ChargerMode::Disconnected,
                ChargerMode::AwaitingStart,
                ChargerMode::Charging,
                ChargerMode::Completed,
                ChargerMode::Error,
                ChargerMode::ReadyToCharge,
                ChargerMode::AwaitingAuthentication,
                ChargerMode::Deauthenticating,
                ChargerMode::Unknown(9),
            ]
        );
        let connected: Vec<_> = modes.iter().map(|m| m.car_connected()).collect();
        assert_eq!(
            connected,
            [
                false, false, true, true, true, false, true, true, false, false
            ]
        );
    }

    #[test]
    fn modes_serialize_in_snake_case() {
        let json = |m| serde_json::to_string(&m).unwrap();
        assert_eq!(json(ChargerMode::ReadyToCharge), r#""ready_to_charge""#);
        assert_eq!(json(ChargerMode::Unknown(42)), r#"{"unknown":42}"#);
    }

    #[test]
    fn keeps_the_newest_reading() {
        let mut c = Charger::new(ChargerConfig::default());
        let newer = reading("2026-11-15T17:00:30Z", ChargerMode::Charging, 2.3);
        c.on_reading(newer);
        c.on_reading(reading("2026-11-15T17:00:00Z", ChargerMode::Completed, 0.0));
        assert_eq!(c.reading, Some(newer));
        let same_time = reading("2026-11-15T17:00:30Z", ChargerMode::Completed, 0.0);
        c.on_reading(same_time);
        assert_eq!(c.reading, Some(same_time));
    }

    #[test]
    fn freshness_and_availability() {
        let mut c = Charger::new(ChargerConfig::default());
        let at = |s| ts(s);
        assert_eq!(c.freshness(at("2026-11-15T17:00:00Z")), Freshness::NoData);
        assert!(!c.available(at("2026-11-15T17:00:00Z")));

        c.on_reading(reading("2026-11-15T17:00:00Z", ChargerMode::Charging, 2.3));
        assert_eq!(c.freshness(at("2026-11-15T17:03:00Z")), Freshness::Fresh);
        assert!(c.available(at("2026-11-15T17:03:00Z")));
        assert_eq!(c.freshness(at("2026-11-15T17:03:01Z")), Freshness::Stale);
        assert!(!c.available(at("2026-11-15T17:03:01Z")));

        let mut offline = reading("2026-11-15T17:00:00Z", ChargerMode::Offline, 0.0);
        c.on_reading(offline);
        assert!(!c.available(at("2026-11-15T17:00:10Z")), "offline");
        offline.mode = ChargerMode::Charging;
        offline.connected = false;
        c.on_reading(offline);
        assert!(!c.available(at("2026-11-15T17:00:10Z")), "no cloud");
    }

    #[test]
    fn power_follows_the_configured_voltage() {
        let c = Charger::new(ChargerConfig::default());
        assert_eq!(c.power_at(Ampere::new(10.0)), Kw::new(2.3));
    }

    #[test]
    fn status_json() {
        let mut c = Charger::new(ChargerConfig::default());
        let now = ts("2026-11-15T17:00:10Z");
        assert_eq!(
            serde_json::to_string(&c.status(now)).unwrap(),
            r#"{"freshness":"no_data","available":false,"reading":null}"#
        );
        let mut r = reading("2026-11-15T17:00:00Z", ChargerMode::Charging, 2.3);
        r.dynamic_current = Some(Ampere::new(10.0));
        c.on_reading(r);
        insta::assert_snapshot!(serde_json::to_string_pretty(&c.status(now)).unwrap());
    }
}

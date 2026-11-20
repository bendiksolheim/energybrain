//! The EV charger (Easee Charge Lite): what it reports, what we know about it, and when to
//! write its current limit.
//!
//! Readings come from polling Easee's cloud (PLAN.md §9), so they are tens of seconds old.
//! The HAN meter stays the truth for the hour's projection; a reading only tells the guard
//! what the car draws and what it is offered.
//!
//! Every limit is written with a time to live, so it clears by itself if energybrain stops
//! refreshing it. The charger offers the car the lowest of its limits, so the user's own
//! "Charger limit" in the app stays the ceiling. A dynamic limit we didn't write is the
//! user's "Temporary charging limit": an override that keeps the guard away from the
//! charger for a while.

use jiff::{SignedDuration, Timestamp};

use crate::energy::Freshness;
use crate::mode::Mode;
use crate::units::{Ampere, Kw, Kwh, Volt};

/// Two currents closer than this are the same setting (Easee works in whole amps).
const SAME_CURRENT: f64 = 0.5;

/// A release is a write of `max_current` that only has to outlive the old limit's last
/// seconds, so it is short.
pub const RELEASE_TTL: SignedDuration = SignedDuration::from_mins(1);

fn same(a: Ampere, b: Ampere) -> bool {
    (a.value() - b.value()).abs() < SAME_CURRENT
}

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

/// What the guard wants from the charger while it limits it.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(tag = "type", content = "current", rename_all = "snake_case")]
pub enum ChargerTarget {
    /// Offer at most this current (whole amps, at least `min_current`).
    Limit(Ampere),
    /// Offer nothing (a dynamic current of 0 A).
    Pause,
}

impl ChargerTarget {
    /// The dynamic current that expresses this target.
    pub const fn current(self) -> Ampere {
        match self {
            Self::Limit(a) => a,
            Self::Pause => Ampere::ZERO,
        }
    }

    /// The target for a whole-amp level, where 0 A means a pause.
    pub fn at(current: Ampere) -> Self {
        if current.value() < SAME_CURRENT {
            Self::Pause
        } else {
            Self::Limit(current)
        }
    }
}

/// A dynamic-current write: `set_dynamic_charger_current { amps, minutes }`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChargerWrite {
    /// The limit (0 A pauses; `max_current` releases).
    pub current: Ampere,
    /// After this the charger clears the limit by itself.
    pub ttl: SignedDuration,
}

/// Our last real write, persisted so a restart neither forgets a limit still in force nor
/// mistakes it for the user's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChargerWriteRecord {
    /// The limit written.
    pub current: Ampere,
    /// When.
    pub at: Timestamp,
    /// When the charger clears it unless refreshed.
    pub until: Timestamp,
}

/// The user's "Temporary charging limit": the guard leaves the charger alone until `until`.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct ChargerOverride {
    /// The user's limit.
    pub current: Ampere,
    /// When the guard may limit the charger again.
    pub until: Timestamp,
}

/// Static configuration of the charger.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChargerConfig {
    /// Power per ampere of offered current: 230 V per phase the car charges on.
    pub voltage: Volt,
    /// With no reading for this long, the charger counts as stale.
    pub stale_after: SignedDuration,
    /// The lowest current a car charges at (IEC 61851: 6 A).
    pub min_current: Ampere,
    /// The charger's own maximum; writing it releases our limit.
    pub max_current: Ampere,
    /// Whether the guard may limit the charger (HomeKit can override at runtime).
    pub shed_allowed: bool,
    /// Whether the guard may pause charging (0 A). Some cars don't resume reliably.
    pub pause_allowed: bool,
    /// Lifetime of each limit; refreshed after half of it.
    pub ttl: SignedDuration,
    /// At most one change per this interval (some cars dislike frequent changes). An
    /// unconfirmed write is retried after it.
    pub min_change_interval: SignedDuration,
    /// The largest raise in one step, in whole amps.
    pub max_raise_step: u8,
    /// A pause lasts at least this long.
    pub min_pause: SignedDuration,
    /// At most this many pauses per hour; after that `min_current` is the floor.
    pub max_pauses_per_hour: u8,
    /// How long the user's temporary limit keeps the guard away.
    pub override_duration: SignedDuration,
    /// After a change the car has reacted, how long the meter's moving average needs.
    pub settle: SignedDuration,
    /// Power above this counts as charging.
    pub charging_threshold: Kw,
}

impl Default for ChargerConfig {
    /// One phase on a 230 V IT net, an Easee's 6–32 A range, and PLAN.md §9's timings.
    fn default() -> Self {
        Self {
            voltage: Volt::new(230.0),
            stale_after: SignedDuration::from_mins(3),
            min_current: Ampere::new(6.0),
            max_current: Ampere::new(32.0),
            shed_allowed: true,
            pause_allowed: false,
            ttl: SignedDuration::from_mins(10),
            min_change_interval: SignedDuration::from_mins(2),
            max_raise_step: 4,
            min_pause: SignedDuration::from_mins(15),
            max_pauses_per_hour: 2,
            override_duration: SignedDuration::from_hours(2),
            settle: SignedDuration::from_mins(2),
            charging_threshold: Kw::new(0.5),
        }
    }
}

/// What to do with the charger this evaluation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChargerActuation {
    /// Nothing.
    None,
    /// Write this limit (active mode).
    Write(ChargerWrite),
    /// Would write this limit (observe mode).
    WouldWrite(ChargerWrite),
    /// Our limit is gone without a write (car unplugged, or the user replaced it): forget it.
    Forget,
}

/// The charger and what is known about it at runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct Charger {
    /// Static configuration.
    pub config: ChargerConfig,
    /// The latest reading.
    pub reading: Option<ChargerReading>,
    /// Our last real write (persisted); `None` once released or cleared.
    pub last_write: Option<ChargerWriteRecord>,
    /// The write before it, which a poll may still show.
    pub previous_write: Option<Ampere>,
    /// Observe mode: the limit we would have in force (`None` = no limit).
    pub last_would_write: Option<Ampere>,
    /// Runtime shed permission from HomeKit (persisted); wins over the config default.
    pub shed_allowed_override: Option<bool>,
    /// The user's latest temporary limit, active or expired (kept so an expired one isn't
    /// detected again).
    pub manual: Option<ChargerOverride>,
}

impl Charger {
    /// A charger nothing has been heard from yet.
    pub const fn new(config: ChargerConfig) -> Self {
        Self {
            config,
            reading: None,
            last_write: None,
            previous_write: None,
            last_would_write: None,
            shed_allowed_override: None,
            manual: None,
        }
    }

    /// Records a reading. One older than the latest (a slow poll overtaken by a faster
    /// one) is ignored. Returns a newly detected user override.
    ///
    /// A dynamic current is the user's when it is below the charger limit, isn't 0 and
    /// isn't one of our last two writes. However a cleared limit reads (absent, 32 A or the
    /// charger limit), it is never mistaken for the user's; a user pause is not recognised.
    pub fn on_reading(&mut self, reading: ChargerReading) -> Option<ChargerOverride> {
        if self.reading.is_some_and(|r| reading.at < r.at) {
            return None;
        }
        self.reading = Some(reading);
        if !reading.mode.car_connected() {
            // Unplugging clears the charger's dynamic current, and with it the override.
            self.manual = None;
            return None;
        }
        let d = reading.dynamic_current?;
        let ceiling = reading.max_current.unwrap_or(self.config.max_current);
        let ours = self.last_write.is_some_and(|w| same(w.current, d))
            || self.previous_write.is_some_and(|p| same(p, d));
        let known = self.manual.is_some_and(|m| same(m.current, d));
        if d.value() < SAME_CURRENT || d >= ceiling || ours || known {
            return None;
        }
        let o = ChargerOverride {
            current: d,
            until: reading.at + self.config.override_duration,
        };
        self.manual = Some(o);
        Some(o)
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

    /// Whether limiting is currently permitted for the charger.
    pub fn shed_allowed(&self) -> bool {
        self.shed_allowed_override
            .unwrap_or(self.config.shed_allowed)
    }

    /// The user's override in force at `now`, if any.
    pub fn active_override(&self, now: Timestamp) -> Option<ChargerOverride> {
        self.manual.filter(|o| o.until > now)
    }

    /// Whether the guard may limit the charger at `now`: permitted and not overridden.
    pub fn sheddable(&self, now: Timestamp) -> bool {
        self.shed_allowed() && self.active_override(now).is_none()
    }

    /// The highest current the car can be offered: the user's charger limit, if known,
    /// within the charger's own maximum.
    pub fn ceiling(&self) -> Ampere {
        self.reading
            .and_then(|r| r.max_current)
            .map_or(self.config.max_current, |m| m.min(self.config.max_current))
    }

    /// The power the car would draw if offered `current`.
    pub fn power_at(&self, current: Ampere) -> Kw {
        current * self.config.voltage
    }

    /// Decides whether to write `target` (`None` = no limit) at `now`.
    ///
    /// In `Active` a limit is written when it differs from our last write, is half-way
    /// through its time to live, or hasn't shown up in a poll within `min_change_interval`
    /// (presumed lost). `None` releases a limit of ours that may still be in force. Nothing
    /// is written while the charger is unavailable; the write happens once it is back.
    /// `Observe` reports what it would write instead; `Off` does nothing.
    pub fn actuate(
        &mut self,
        mode: Mode,
        target: Option<ChargerTarget>,
        now: Timestamp,
    ) -> ChargerActuation {
        let Some(r) = self.reading else {
            return ChargerActuation::None;
        };
        if !r.mode.car_connected() || self.active_override(now).is_some() {
            self.last_would_write = None;
            self.previous_write = self.last_write.map(|w| w.current).or(self.previous_write);
            return match self.last_write.take() {
                Some(_) => ChargerActuation::Forget,
                None => ChargerActuation::None,
            };
        }
        if !self.available(now) {
            return ChargerActuation::None;
        }
        let want = target.map(ChargerTarget::current);
        match mode {
            Mode::Off => ChargerActuation::None,
            Mode::Observe => {
                if want.is_some_and(|w| self.last_would_write.is_some_and(|l| same(l, w)))
                    || (want.is_none() && self.last_would_write.is_none())
                {
                    return ChargerActuation::None;
                }
                self.last_would_write = want;
                ChargerActuation::WouldWrite(self.write_for(want))
            }
            Mode::Active => match want {
                Some(current) => {
                    let due = self.last_write.is_none_or(|w| {
                        let confirmed =
                            r.at > w.at && r.dynamic_current.is_some_and(|d| same(d, w.current));
                        !same(w.current, current)
                            || now.duration_since(w.at) >= self.config.ttl / 2
                            || (!confirmed
                                && now.duration_since(w.at) >= self.config.min_change_interval)
                    });
                    if !due {
                        return ChargerActuation::None;
                    }
                    self.previous_write = self.last_write.map(|w| w.current);
                    self.last_write = Some(ChargerWriteRecord {
                        current,
                        at: now,
                        until: now + self.config.ttl,
                    });
                    ChargerActuation::Write(self.write_for(Some(current)))
                }
                None => match self.last_write.take() {
                    Some(w) => {
                        self.previous_write = Some(w.current);
                        if w.until > now {
                            ChargerActuation::Write(self.write_for(None))
                        } else {
                            ChargerActuation::Forget
                        }
                    }
                    None => ChargerActuation::None,
                },
            },
        }
    }

    fn write_for(&self, current: Option<Ampere>) -> ChargerWrite {
        match current {
            Some(current) => ChargerWrite {
                current,
                ttl: self.config.ttl,
            },
            None => ChargerWrite {
                current: self.config.max_current,
                ttl: RELEASE_TTL,
            },
        }
    }

    /// Status for publishing, with the guard's target.
    pub fn status(&self, now: Timestamp, limit: Option<ChargerTarget>) -> ChargerStatus {
        ChargerStatus {
            freshness: self.freshness(now),
            available: self.available(now),
            reading: self.reading,
            limit,
            shed_allowed: self.shed_allowed(),
            override_until: self.active_override(now).map(|o| o.until),
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
    /// The guard's limit, if it limits the charger.
    pub limit: Option<ChargerTarget>,
    /// Whether limiting is allowed.
    pub shed_allowed: bool,
    /// Active override end, if any.
    pub override_until: Option<Timestamp>,
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

    fn a(v: f64) -> Ampere {
        Ampere::new(v)
    }

    fn charging_at(at: &str) -> Charger {
        let mut c = Charger::new(ChargerConfig::default());
        c.on_reading(reading(at, ChargerMode::Charging, 3.4));
        c
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
    fn modes_and_targets_serialize_in_snake_case() {
        let json = |v: &dyn erased::Ser| v.json();
        assert_eq!(json(&ChargerMode::ReadyToCharge), r#""ready_to_charge""#);
        assert_eq!(json(&ChargerMode::Unknown(42)), r#"{"unknown":42}"#);
        assert_eq!(json(&ChargerTarget::Pause), r#"{"type":"pause"}"#);
        assert_eq!(
            json(&ChargerTarget::Limit(a(9.0))),
            r#"{"type":"limit","current":{"value":9.0,"unit":"A"}}"#
        );
    }

    /// `dyn Serialize` without the erased-serde crate, for the test above.
    mod erased {
        pub trait Ser {
            fn json(&self) -> String;
        }
        impl<T: serde::Serialize> Ser for T {
            fn json(&self) -> String {
                serde_json::to_string(self).unwrap()
            }
        }
    }

    #[test]
    fn targets_map_to_currents() {
        assert_eq!(ChargerTarget::at(a(0.0)), ChargerTarget::Pause);
        assert_eq!(ChargerTarget::at(a(6.0)), ChargerTarget::Limit(a(6.0)));
        assert_eq!(ChargerTarget::Pause.current(), Ampere::ZERO);
        assert_eq!(ChargerTarget::Limit(a(7.0)).current(), a(7.0));
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
    fn power_ceiling_and_permissions() {
        let mut c = Charger::new(ChargerConfig::default());
        assert_eq!(c.power_at(a(10.0)), Kw::new(2.3));
        assert_eq!(
            c.ceiling(),
            a(32.0),
            "no reading: the charger's own maximum"
        );
        c.on_reading(reading("2026-11-15T17:00:00Z", ChargerMode::Charging, 2.3));
        assert_eq!(c.ceiling(), a(15.0), "the user's charger limit");
        let now = ts("2026-11-15T17:00:10Z");
        assert!(c.sheddable(now));
        c.shed_allowed_override = Some(false);
        assert!(!c.sheddable(now));
        c.shed_allowed_override = None;
        c.manual = Some(ChargerOverride {
            current: a(8.0),
            until: ts("2026-11-15T17:00:11Z"),
        });
        assert!(!c.sheddable(now));
        assert!(c.sheddable(ts("2026-11-15T17:00:11Z")), "expired");
    }

    #[test]
    fn detects_the_users_temporary_limit_but_not_ours_or_a_cleared_one() {
        let mut c = Charger::new(ChargerConfig::default());
        let with = |d: Option<f64>, at: &str| {
            let mut r = reading(at, ChargerMode::Charging, 2.3);
            r.dynamic_current = d.map(a);
            r
        };
        // A cleared limit, whatever it reads as.
        for d in [None, Some(0.0), Some(15.0), Some(32.0)] {
            assert_eq!(c.on_reading(with(d, "2026-11-15T17:00:00Z")), None, "{d:?}");
        }
        // Ours, and the one before it (a poll may lag a write).
        c.last_write = Some(ChargerWriteRecord {
            current: a(9.0),
            at: ts("2026-11-15T16:59:00Z"),
            until: ts("2026-11-15T17:09:00Z"),
        });
        c.previous_write = Some(a(11.0));
        assert_eq!(c.on_reading(with(Some(9.0), "2026-11-15T17:00:10Z")), None);
        assert_eq!(c.on_reading(with(Some(11.0), "2026-11-15T17:00:20Z")), None);
        // The user's: an override for two hours, reported once.
        let o = c.on_reading(with(Some(8.0), "2026-11-15T17:00:30Z"));
        assert_eq!(
            o,
            Some(ChargerOverride {
                current: a(8.0),
                until: ts("2026-11-15T19:00:30Z"),
            })
        );
        assert_eq!(c.on_reading(with(Some(8.0), "2026-11-15T17:01:00Z")), None);
        // Still the same limit after it expired: not a new override.
        assert_eq!(c.on_reading(with(Some(8.0), "2026-11-15T19:01:00Z")), None);
        assert!(c.active_override(ts("2026-11-15T19:01:00Z")).is_none());
        // Unplugging ends it.
        c.on_reading(reading(
            "2026-11-15T19:02:00Z",
            ChargerMode::Disconnected,
            0.0,
        ));
        assert_eq!(c.manual, None);
    }

    #[test]
    fn active_writes_refreshes_retries_and_releases() {
        let mut c = charging_at("2026-11-15T17:00:00Z");
        let t = |s: &str| ts(&format!("2026-11-15T{s}Z"));
        let limit = Some(ChargerTarget::Limit(a(9.0)));
        let write = |current: f64, ttl: i64| {
            ChargerActuation::Write(ChargerWrite {
                current: a(current),
                ttl: SignedDuration::from_mins(ttl),
            })
        };

        assert_eq!(
            c.actuate(Mode::Active, limit, t("17:00:10")),
            write(9.0, 10)
        );
        assert_eq!(
            c.last_write,
            Some(ChargerWriteRecord {
                current: a(9.0),
                at: t("17:00:10"),
                until: t("17:10:10"),
            })
        );
        // Unconfirmed, but not presumed lost yet.
        assert_eq!(
            c.actuate(Mode::Active, limit, t("17:01:00")),
            ChargerActuation::None
        );
        // Confirmed by a poll: no retry, only the refresh at half the TTL.
        let mut r = reading("2026-11-15T17:00:40Z", ChargerMode::Charging, 2.1);
        r.dynamic_current = Some(a(9.0));
        c.on_reading(r);
        assert_eq!(
            c.actuate(Mode::Active, limit, t("17:03:00")),
            ChargerActuation::None
        );
        c.reading.as_mut().unwrap().at = t("17:05:00");
        assert_eq!(
            c.actuate(Mode::Active, limit, t("17:05:10")),
            write(9.0, 10)
        );
        // Unconfirmed after `min_change_interval`: presumed lost, retried.
        c.reading.as_mut().unwrap().at = t("17:06:00");
        assert_eq!(
            c.actuate(Mode::Active, limit, t("17:07:00")),
            ChargerActuation::None,
            "the 17:06 poll still shows 9 A"
        );
        c.reading.as_mut().unwrap().dynamic_current = None;
        assert_eq!(
            c.actuate(Mode::Active, limit, t("17:07:10")),
            write(9.0, 10)
        );
        // A different target is written at once; the old one is still "ours".
        let seven = Some(ChargerTarget::Limit(a(7.0)));
        assert_eq!(
            c.actuate(Mode::Active, seven, t("17:07:20")),
            write(7.0, 10)
        );
        assert_eq!(c.previous_write, Some(a(9.0)));
        // No target: release with the charger's maximum and a short TTL.
        assert_eq!(c.actuate(Mode::Active, None, t("17:07:30")), write(32.0, 1));
        assert_eq!(c.last_write, None);
        assert_eq!(c.previous_write, Some(a(7.0)));
        assert_eq!(
            c.actuate(Mode::Active, None, t("17:07:40")),
            ChargerActuation::None
        );
        // An expired limit needs no release, only forgetting.
        c.last_write = Some(ChargerWriteRecord {
            current: a(7.0),
            at: t("16:00:00"),
            until: t("16:10:00"),
        });
        assert_eq!(
            c.actuate(Mode::Active, None, t("17:07:50")),
            ChargerActuation::Forget
        );
        // Off writes nothing.
        assert_eq!(
            c.actuate(Mode::Off, limit, t("17:08:00")),
            ChargerActuation::None
        );
    }

    #[test]
    fn nothing_is_written_without_a_reachable_charger() {
        let mut c = Charger::new(ChargerConfig::default());
        let limit = Some(ChargerTarget::Pause);
        let now = ts("2026-11-15T17:00:00Z");
        assert_eq!(c.actuate(Mode::Active, limit, now), ChargerActuation::None);
        c.on_reading(reading("2026-11-15T16:50:00Z", ChargerMode::Charging, 2.3));
        assert_eq!(
            c.actuate(Mode::Active, limit, now),
            ChargerActuation::None,
            "stale"
        );
    }

    #[test]
    fn unplugging_or_an_override_forgets_our_limit_without_writing() {
        let mut c = charging_at("2026-11-15T17:00:00Z");
        let limit = Some(ChargerTarget::Limit(a(9.0)));
        let now = ts("2026-11-15T17:00:10Z");
        c.actuate(Mode::Active, limit, now);
        c.on_reading(reading(
            "2026-11-15T17:00:20Z",
            ChargerMode::Disconnected,
            0.0,
        ));
        assert_eq!(c.actuate(Mode::Active, None, now), ChargerActuation::Forget);
        assert_eq!(c.previous_write, Some(a(9.0)));
        assert_eq!(c.actuate(Mode::Active, None, now), ChargerActuation::None);

        let mut c = charging_at("2026-11-15T17:00:00Z");
        c.actuate(Mode::Observe, limit, now);
        c.manual = Some(ChargerOverride {
            current: a(8.0),
            until: ts("2026-11-15T19:00:00Z"),
        });
        assert_eq!(c.actuate(Mode::Observe, limit, now), ChargerActuation::None);
        assert_eq!(c.last_would_write, None);
    }

    #[test]
    fn observe_reports_each_change_once() {
        let mut c = charging_at("2026-11-15T17:00:00Z");
        let now = ts("2026-11-15T17:00:10Z");
        let limit = Some(ChargerTarget::Limit(a(9.0)));
        let would = |current: f64, ttl: i64| {
            ChargerActuation::WouldWrite(ChargerWrite {
                current: a(current),
                ttl: SignedDuration::from_mins(ttl),
            })
        };
        assert_eq!(c.actuate(Mode::Observe, None, now), ChargerActuation::None);
        assert_eq!(c.actuate(Mode::Observe, limit, now), would(9.0, 10));
        assert_eq!(c.actuate(Mode::Observe, limit, now), ChargerActuation::None);
        assert_eq!(
            c.actuate(Mode::Observe, Some(ChargerTarget::Pause), now),
            would(0.0, 10)
        );
        assert_eq!(c.actuate(Mode::Observe, None, now), would(32.0, 1));
        assert_eq!(c.last_write, None, "observe never writes");
    }

    #[test]
    fn status_json() {
        let mut c = Charger::new(ChargerConfig::default());
        let now = ts("2026-11-15T17:00:10Z");
        assert_eq!(
            serde_json::to_string(&c.status(now, None)).unwrap(),
            r#"{"freshness":"no_data","available":false,"reading":null,"limit":null,"shed_allowed":true,"override_until":null}"#
        );
        c.last_write = Some(ChargerWriteRecord {
            current: a(10.0),
            at: ts("2026-11-15T16:59:00Z"),
            until: ts("2026-11-15T17:09:00Z"),
        });
        let mut r = reading("2026-11-15T17:00:00Z", ChargerMode::Charging, 2.3);
        r.dynamic_current = Some(Ampere::new(10.0));
        assert_eq!(c.on_reading(r), None, "our own limit");
        let limit = Some(ChargerTarget::Limit(a(10.0)));
        insta::assert_snapshot!(serde_json::to_string_pretty(&c.status(now, limit)).unwrap());
    }
}

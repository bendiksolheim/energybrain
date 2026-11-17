//! What goes into and comes out of the engine.

use jiff::Timestamp;

use crate::energy::{Freshness, HourEnergy};
use crate::guard::{GuardEvent, GuardStatus};
use crate::ids::ZoneId;
use crate::mode::Mode;
use crate::peaks::{DayPeak, HourLimit};
use crate::policy::Reason;
use crate::tariff::Tier;
use crate::time::HourStart;
use crate::units::{Celsius, Kw, Kwh};
use crate::zone::Override;

/// Something that happened in the outside world.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// Cumulative import register reading (normally hourly), at the meter's capture time.
    MeterCumulative {
        /// Capture time.
        at: Timestamp,
        /// Register value.
        total: Kwh,
    },
    /// Instant import power (every few seconds).
    MeterPower {
        /// Receive time.
        at: Timestamp,
        /// Power.
        power: Kw,
    },
    /// A thermostat reported its setpoint.
    ThermostatSetpoint {
        /// Zone.
        zone: ZoneId,
        /// Receive time.
        at: Timestamp,
        /// Reported setpoint.
        value: Celsius,
    },
    /// A thermostat reported its power.
    ZonePower {
        /// Zone.
        zone: ZoneId,
        /// Receive time.
        at: Timestamp,
        /// Power.
        power: Kw,
    },
    /// A thermostat reported its cumulative energy counter.
    ZoneEnergy {
        /// Zone.
        zone: ZoneId,
        /// Receive time.
        at: Timestamp,
        /// Counter value.
        total: Kwh,
    },
    /// The user changed the mode.
    SetMode {
        /// When.
        at: Timestamp,
        /// New mode.
        mode: Mode,
    },
    /// The user allowed or forbade shedding a zone.
    SetShedAllowed {
        /// Zone.
        zone: ZoneId,
        /// Whether shedding is allowed.
        allowed: bool,
    },
    /// Periodic evaluation.
    Tick {
        /// Now.
        at: Timestamp,
    },
}

/// A side effect the engine asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// Write a setpoint to a thermostat (active mode only).
    WriteSetpoint {
        /// Zone.
        zone: ZoneId,
        /// Setpoint.
        value: Celsius,
        /// Why.
        reasons: Vec<Reason>,
    },
    /// Observe mode: this write would have happened.
    WouldWrite {
        /// Zone.
        zone: ZoneId,
        /// Setpoint.
        value: Celsius,
        /// Why.
        reasons: Vec<Reason>,
    },
    /// Publish the current status (MQTT, HTTP, metrics).
    PublishStatus(Box<Status>),
    /// Store an hour's energy (upsert).
    PersistHour(HourEnergy),
    /// Store our last write to a zone.
    PersistZoneWrite {
        /// Zone.
        zone: ZoneId,
        /// Setpoint written.
        value: Celsius,
        /// When.
        at: Timestamp,
    },
    /// Store a zone's learned base setpoint.
    PersistZoneBase {
        /// Zone.
        zone: ZoneId,
        /// Base.
        value: Celsius,
    },
    /// Store or clear a zone's override.
    PersistOverride {
        /// Zone.
        zone: ZoneId,
        /// The override, or `None` when it ended.
        manual: Option<Override>,
    },
    /// Store a zone's energy for one hour (from its own counter).
    PersistZoneHour {
        /// Zone.
        zone: ZoneId,
        /// Hour.
        hour: HourStart,
        /// Energy.
        kwh: Kwh,
    },
    /// Store a zone's learned heating power.
    PersistZoneStats {
        /// Zone.
        zone: ZoneId,
        /// Learned power while heating.
        heating_kw: Kw,
    },
    /// Store the mode.
    PersistMode(Mode),
    /// Store a zone's runtime shed permission.
    PersistShedAllowed {
        /// Zone.
        zone: ZoneId,
        /// Whether shedding is allowed.
        allowed: bool,
    },
    /// Append to the event log.
    PersistEvent(Event),
}

/// An entry in the event log.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Event {
    /// When.
    pub at: Timestamp,
    /// Mode at the time (so observe-mode sheds read as "would shed").
    pub mode: Mode,
    /// What happened.
    pub kind: EventKind,
}

/// What an [`Event`] records.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// A guard decision.
    Guard {
        /// What the guard did.
        event: GuardEvent,
    },
    /// The user overrode a shed zone.
    Override {
        /// Zone.
        zone: ZoneId,
        /// The user's setpoint (`setpoint` in JSON, which would otherwise read
        /// `"value": {"value": …}`).
        #[serde(rename = "setpoint")]
        value: Celsius,
        /// When it ends.
        until: Timestamp,
    },
    /// An override ended.
    OverrideExpired {
        /// Zone.
        zone: ZoneId,
    },
    /// The mode changed.
    ModeChanged {
        /// Previous mode.
        from: Mode,
        /// New mode.
        to: Mode,
    },
}

/// Everything worth showing about the current state.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Status {
    /// When this status was computed.
    pub at: Timestamp,
    /// Mode.
    pub mode: Mode,
    /// Guard state.
    pub guard: GuardStatus,
    /// Running hour.
    pub hour: HourStart,
    /// Energy used so far this hour.
    pub used: Kwh,
    /// Projection for the hour.
    pub projected: Option<Kwh>,
    /// Moving-average power.
    pub power: Option<Kw>,
    /// Meter freshness.
    pub freshness: Freshness,
    /// When the meter last reported power.
    pub meter_last_seen: Option<Timestamp>,
    /// This hour's limit.
    pub limit: HourLimit,
    /// The month's top days, highest first.
    pub top_days: Vec<DayPeak>,
    /// Their average.
    pub top_average: Kw,
    /// The tier that average falls in.
    pub tier: Tier,
    /// Per-zone state.
    pub zones: Vec<ZoneStatus>,
}

/// One zone's part of [`Status`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ZoneStatus {
    /// Zone.
    pub id: ZoneId,
    /// Display name.
    pub name: String,
    /// The user's setpoint.
    pub base: Option<Celsius>,
    /// What the thermostat reports.
    pub reported: Option<Celsius>,
    /// What the engine wants.
    pub decision: Option<Celsius>,
    /// Why.
    pub reasons: Vec<Reason>,
    /// Whether the guard sheds it.
    pub shed: bool,
    /// Whether shedding is allowed.
    pub shed_allowed: bool,
    /// Active override end, if any.
    pub override_until: Option<Timestamp>,
    /// Last reported power.
    pub power: Option<Kw>,
    /// Learned heating power (`heating_power` in JSON, where the unit is part of the value).
    #[serde(rename = "heating_power")]
    pub heating_kw: Option<Kw>,
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;
    use crate::energy::Quality;
    use crate::guard::ReleaseReason;
    use crate::peaks::LimitBasis;
    use crate::policy::PolicyName;
    use crate::tariff::TierIndex;
    use crate::units::Kr;
    use crate::zone::tests::ts;

    fn zone() -> ZoneId {
        ZoneId::new("bad").unwrap()
    }

    fn json<T: serde::Serialize>(v: &T) -> String {
        serde_json::to_string_pretty(v).unwrap()
    }

    #[test]
    fn status_json() {
        let hour = HourStart::containing(ts("2026-11-15T17:00:00Z"));
        let status = Status {
            at: ts("2026-11-15T17:20:00Z"),
            mode: Mode::Observe,
            guard: GuardStatus::Shedding,
            hour,
            used: Kwh::new(1.5),
            projected: Some(Kwh::new(5.2)),
            power: Some(Kw::new(6.1)),
            freshness: Freshness::Fresh,
            meter_last_seen: Some(ts("2026-11-15T17:19:58Z")),
            limit: HourLimit {
                allowed: Kwh::new(7.0),
                effective: Kwh::new(6.7),
                basis: LimitBasis::TodayAlreadyHigher {
                    today_max: Kwh::new(7.0),
                },
            },
            top_days: vec![crate::peaks::DayPeak {
                date: date(2026, 11, 15),
                hour,
                kwh: Kwh::new(7.0),
            }],
            top_average: Kw::new(7.0),
            tier: Tier {
                index: TierIndex(2),
                from: Kw::new(5.0),
                to: Some(Kw::new(10.0)),
                price: Kr::new(397.0),
            },
            zones: vec![ZoneStatus {
                id: zone(),
                name: "Bad".to_owned(),
                base: Some(Celsius::new(26.5)),
                reported: Some(Celsius::new(26.5)),
                decision: Some(Celsius::new(22.5)),
                reasons: vec![Reason {
                    policy: PolicyName::PeakGuard,
                    detail: "shed to 22.5 °C".to_owned(),
                }],
                shed: true,
                shed_allowed: true,
                override_until: None,
                power: Some(Kw::new(1.5)),
                heating_kw: Some(Kw::new(1.45)),
            }],
        };
        insta::assert_snapshot!(json(&status));
        let tier_bound = json(&LimitBasis::TierBound);
        assert_eq!(tier_bound, "{\n  \"type\": \"tier_bound\"\n}");
    }

    #[test]
    fn event_json() {
        let at = ts("2026-11-15T17:20:00Z");
        let kinds = vec![
            EventKind::Guard {
                event: GuardEvent::Shed {
                    zone: zone(),
                    load: Kw::new(1.5),
                    projected: Kwh::new(5.2),
                    limit: Kwh::new(4.7),
                },
            },
            EventKind::Guard {
                event: GuardEvent::Release {
                    zone: zone(),
                    reason: ReleaseReason::BelowLimit,
                },
            },
            EventKind::Guard {
                event: GuardEvent::LimitUnreachable {
                    projected: Kwh::new(9.0),
                    limit: Kwh::new(4.7),
                    available: Kwh::new(1.0),
                },
            },
            EventKind::Override {
                zone: zone(),
                value: Celsius::new(27.0),
                until: ts("2026-11-15T19:20:00Z"),
            },
            EventKind::OverrideExpired { zone: zone() },
            EventKind::ModeChanged {
                from: Mode::Active,
                to: Mode::Off,
            },
        ];
        let events: Vec<_> = kinds
            .into_iter()
            .map(|kind| Event {
                at,
                mode: Mode::Active,
                kind,
            })
            .collect();
        insta::assert_snapshot!(json(&events));
    }

    #[test]
    fn remaining_enums_serialize_in_snake_case() {
        let all = [
            json(&GuardStatus::Idle),
            json(&GuardStatus::LimitUnreachable),
            json(&GuardStatus::MeterStale),
            json(&GuardStatus::Disabled),
            json(&Freshness::NoData),
            json(&Freshness::Stale),
            json(&ReleaseReason::MeterStale),
            json(&ReleaseReason::NotSheddable),
            json(&ReleaseReason::GuardOff),
            json(&PolicyName::UserSetpointBase),
            json(&PolicyName::ManualOverride),
            json(&Mode::Active),
            json(&Quality::Estimated),
            json(&Quality::Interpolated),
            json(&Quality::Measured),
        ];
        assert_eq!(
            all.join(" "),
            r#""idle" "limit_unreachable" "meter_stale" "disabled" "no_data" "stale" "meter_stale" "not_sheddable" "guard_off" "user_setpoint_base" "manual_override" "active" "estimated" "interpolated" "measured""#
        );
        let e = crate::energy::HourEnergy {
            hour: HourStart::containing(ts("2026-11-15T17:00:00Z")),
            kwh: Kwh::new(4.2),
            quality: Quality::Measured,
        };
        assert_eq!(
            serde_json::to_string(&e).unwrap(),
            r#"{"hour":"2026-11-15T17:00:00Z","energy":{"value":4.2,"unit":"kWh"},"quality":"measured"}"#
        );
    }
}

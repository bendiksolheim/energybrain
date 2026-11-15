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
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// When.
    pub at: Timestamp,
    /// Mode at the time (so observe-mode sheds read as "would shed").
    pub mode: Mode,
    /// What happened.
    pub kind: EventKind,
}

/// What an [`Event`] records.
#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    /// A guard decision.
    Guard(GuardEvent),
    /// The user overrode a shed zone.
    Override {
        /// Zone.
        zone: ZoneId,
        /// The user's setpoint.
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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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
    /// Learned heating power.
    pub heating_kw: Option<Kw>,
}

//! The engine: single owner of all state. `handle` turns one [`Input`] into [`Output`]s.
//!
//! Only `Tick` evaluates the guard and the policies; other inputs just update state (and
//! persist what they learned). Time always comes from the input.

use std::collections::BTreeMap;

use jiff::Timestamp;

use crate::actuator::{Actuation, ActuatorConfig, actuate};
use crate::arbiter::{Decision, arbitrate};
use crate::charger::{Charger, ChargerConfig};
use crate::energy::{HourEnergy, Meter, MeterConfig, MeterSnapshot};
use crate::guard::{GuardConfig, GuardEvent, GuardInput, GuardState, ReleaseReason, Shed};
use crate::ids::ZoneId;
use crate::io::{Event, EventKind, Input, Output, Status, ZoneStatus};
use crate::mode::Mode;
use crate::peaks::{HourLimit, MonthLedger, hour_limit};
use crate::policy::{Context, ManualOverride, PeakGuardCaps, Policy, Proposal, UserSetpointBase};
use crate::tariff::CapacityTariff;
use crate::time::{HourStart, LocalCalendar};
use crate::units::{Celsius, Kw, Kwh};
use crate::zone::{Override, SetpointReport, Zone, ZoneConfig, ZoneSettings};

/// Everything the engine is configured with.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Local calendar for day/month bucketing.
    pub calendar: LocalCalendar,
    /// The capacity tariff (for status).
    pub tariff: CapacityTariff,
    /// Upper bound of the target tier.
    pub target: Kw,
    /// Subtracted from the allowed energy to get the effective limit.
    pub safety_margin: Kwh,
    /// Meter model tuning.
    pub meter: MeterConfig,
    /// Guard tuning.
    pub guard: GuardConfig,
    /// Actuator tuning.
    pub actuator: ActuatorConfig,
    /// Zone tuning.
    pub zone_settings: ZoneSettings,
    /// The zones.
    pub zones: Vec<ZoneConfig>,
    /// The EV charger, if there is one.
    pub charger: Option<ChargerConfig>,
}

/// What is persisted for one zone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PersistedZone {
    /// Learned base setpoint.
    pub base: Option<Celsius>,
    /// Our last write.
    pub last_write: Option<(Celsius, Timestamp)>,
    /// Learned heating power.
    pub heating_kw: Option<Kw>,
    /// Runtime shed permission.
    pub shed_allowed: Option<bool>,
    /// Override.
    pub manual: Option<Override>,
}

/// State loaded at startup.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PersistedState {
    /// Last mode; `None` on first ever start (then `Observe`).
    pub mode: Option<Mode>,
    /// Stored hours (at least the current month).
    pub hours: BTreeMap<HourStart, HourEnergy>,
    /// Stored zone state.
    pub zones: BTreeMap<ZoneId, PersistedZone>,
}

impl PersistedState {
    /// Applies the persistence part of an output (what a store does), ignoring the rest.
    /// Useful for in-memory stores and restart tests.
    pub fn apply(&mut self, output: &Output) {
        match output {
            Output::PersistHour(e) => {
                self.hours.insert(e.hour, *e);
            }
            Output::PersistZoneWrite { zone, value, at } => {
                self.zone(zone).last_write = Some((*value, *at));
            }
            Output::PersistZoneBase { zone, value } => self.zone(zone).base = Some(*value),
            Output::PersistOverride { zone, manual } => self.zone(zone).manual = *manual,
            Output::PersistZoneStats { zone, heating_kw } => {
                self.zone(zone).heating_kw = Some(*heating_kw);
            }
            Output::PersistMode(mode) => self.mode = Some(*mode),
            Output::PersistShedAllowed { zone, allowed } => {
                self.zone(zone).shed_allowed = Some(*allowed);
            }
            Output::WriteSetpoint { .. }
            | Output::WouldWrite { .. }
            | Output::PublishStatus(_)
            | Output::PersistZoneHour { .. }
            | Output::PersistEvent(_) => {}
        }
    }

    fn zone(&mut self, id: &ZoneId) -> &mut PersistedZone {
        self.zones.entry(id.clone()).or_default()
    }
}

/// The decision core.
pub struct Engine {
    cfg: EngineConfig,
    mode: Mode,
    meter: Meter,
    ledger: MonthLedger,
    zones: BTreeMap<ZoneId, Zone>,
    guard: GuardState,
    charger: Option<Charger>,
    policies: Vec<Box<dyn Policy + Send + Sync>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("mode", &self.mode)
            .field("zones", &self.zones.len())
            .field("guard", &self.guard)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Builds the engine from configuration and persisted state.
    ///
    /// Startup safety: in `Active` mode, a zone whose last write is its shed cap is taken
    /// back into the shed set (shed since that write), so the guard decides whether to keep
    /// or release it; every other zone is driven to its base by the first evaluation.
    pub fn new(cfg: EngineConfig, persisted: PersistedState, now: Timestamp) -> Self {
        let mode = persisted.mode.unwrap_or_default();
        let mut ledger = MonthLedger::new(cfg.calendar.month_of(HourStart::containing(now)));
        for e in persisted.hours.into_values() {
            ledger.upsert(&cfg.calendar, e);
        }
        let deadband = cfg.actuator.deadband;
        let mut guard = GuardState::default();
        let mut zones = BTreeMap::new();
        for zc in &cfg.zones {
            let mut z = Zone::new(zc.clone());
            if let Some(p) = persisted.zones.get(&zc.id) {
                z.base = p.base;
                z.last_write = p.last_write;
                z.heating_kw = p.heating_kw;
                z.shed_allowed_override = p.shed_allowed;
                z.manual = p.manual;
            }
            let left_shed = match (z.last_write, z.shed_cap(), z.base) {
                (Some((w, _)), Some(cap), Some(base)) => {
                    (w - cap).abs() <= deadband && (w - base).abs() > deadband
                }
                _ => false,
            };
            if mode == Mode::Active
                && left_shed
                && z.sheddable(now)
                && let Some((_, since)) = z.last_write
            {
                guard.shed.push(Shed {
                    zone: z.id().clone(),
                    since,
                    load: z.load(),
                    reason: "still shed when the service restarted".to_owned(),
                });
            }
            zones.insert(z.id().clone(), z);
        }
        let policies: Vec<Box<dyn Policy + Send + Sync>> = vec![
            Box::new(UserSetpointBase),
            Box::new(PeakGuardCaps),
            Box::new(ManualOverride),
        ];
        Self {
            meter: Meter::new(cfg.meter),
            charger: cfg.charger.map(Charger::new),
            cfg,
            mode,
            ledger,
            zones,
            guard,
            policies,
        }
    }

    /// The current mode.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Processes one input.
    pub fn handle(&mut self, input: Input) -> Vec<Output> {
        match input {
            Input::MeterPower { at, power } => {
                self.meter.record_power(at, power);
                Vec::new()
            }
            Input::MeterCumulative { at, total } => {
                let settled = self.meter.record_cumulative(at, total);
                settled.into_iter().map(|e| self.record_hour(e)).collect()
            }
            Input::ThermostatSetpoint { zone, at, value } => self.on_setpoint(&zone, at, value),
            Input::ZonePower { zone, at, power } => {
                let settings = self.cfg.zone_settings;
                self.zones
                    .get_mut(&zone)
                    .and_then(|z| z.on_power(at, power, &settings))
                    .map(|heating_kw| Output::PersistZoneStats { zone, heating_kw })
                    .into_iter()
                    .collect()
            }
            Input::ZoneEnergy { zone, at, total } => self
                .zones
                .get_mut(&zone)
                .and_then(|z| z.on_energy(at, total))
                .map(|(hour, kwh)| Output::PersistZoneHour { zone, hour, kwh })
                .into_iter()
                .collect(),
            Input::Charger(reading) => {
                if let Some(c) = &mut self.charger {
                    c.on_reading(reading);
                }
                Vec::new()
            }
            Input::SetMode { at, mode } => self.set_mode(at, mode),
            Input::SetShedAllowed { zone, allowed } => match self.zones.get_mut(&zone) {
                Some(z) => {
                    z.shed_allowed_override = Some(allowed);
                    vec![Output::PersistShedAllowed { zone, allowed }]
                }
                None => Vec::new(),
            },
            Input::Tick { at } => self.evaluate(at),
        }
    }

    fn record_hour(&mut self, e: HourEnergy) -> Output {
        self.ledger.upsert(&self.cfg.calendar, e);
        Output::PersistHour(e)
    }

    fn event(&self, at: Timestamp, kind: EventKind) -> Output {
        Output::PersistEvent(Event {
            at,
            mode: self.mode,
            kind,
        })
    }

    fn on_setpoint(&mut self, id: &ZoneId, at: Timestamp, value: Celsius) -> Vec<Output> {
        let shed = self.guard.is_shed(id);
        let Some(z) = self.zones.get_mut(id) else {
            return Vec::new();
        };
        let cap = if shed { z.shed_cap() } else { None };
        match z.on_setpoint(at, value, cap, &self.cfg.zone_settings) {
            SetpointReport::Echo => Vec::new(),
            SetpointReport::NewBase(value) => vec![Output::PersistZoneBase {
                zone: id.clone(),
                value,
            }],
            SetpointReport::Override(o) => vec![
                Output::PersistZoneBase {
                    zone: id.clone(),
                    value: o.value,
                },
                Output::PersistOverride {
                    zone: id.clone(),
                    manual: Some(o),
                },
                self.event(
                    at,
                    EventKind::Override {
                        zone: id.clone(),
                        value: o.value,
                        until: o.until,
                    },
                ),
            ],
        }
    }

    /// Restores every shed zone to its base before the service stops (Active mode only;
    /// otherwise nothing was written). The persisted mode is unchanged, and the guard sheds
    /// again after a restart if it still needs to.
    pub fn shutdown(&mut self, at: Timestamp) -> Vec<Output> {
        if self.mode != Mode::Active {
            return Vec::new();
        }
        let mut out = Vec::new();
        for s in std::mem::take(&mut self.guard.shed) {
            out.push(self.event(
                at,
                EventKind::Guard {
                    event: GuardEvent::Release {
                        zone: s.zone,
                        reason: ReleaseReason::GuardOff,
                    },
                },
            ));
        }
        let decisions = self.decide(at);
        out.extend(self.actuate_all(at, &decisions, Mode::Active));
        out
    }

    fn set_mode(&mut self, at: Timestamp, new: Mode) -> Vec<Output> {
        let old = self.mode;
        if new == old {
            return Vec::new();
        }
        // Undo every shed as the last act of Active mode.
        let mut out = self.shutdown(at);
        if new == Mode::Active {
            for z in self.zones.values_mut() {
                z.last_would_write = None;
            }
        }
        self.mode = new;
        out.push(Output::PersistMode(new));
        out.push(self.event(at, EventKind::ModeChanged { from: old, to: new }));
        out
    }

    fn evaluate(&mut self, now: Timestamp) -> Vec<Output> {
        let mut out = Vec::new();
        if let Some(e) = self.meter.advance(now) {
            out.push(self.record_hour(e));
        }
        let hour = HourStart::containing(now);
        self.ledger.roll_to(self.cfg.calendar.month_of(hour));

        let expired: Vec<ZoneId> = self
            .zones
            .values_mut()
            .filter(|z| z.manual.is_some_and(|o| o.until <= now))
            .map(|z| {
                z.manual = None;
                z.id().clone()
            })
            .collect();
        for zone in expired {
            out.push(Output::PersistOverride {
                zone: zone.clone(),
                manual: None,
            });
            out.push(self.event(now, EventKind::OverrideExpired { zone }));
        }

        let snap = self.meter.snapshot(now);
        let limit = hour_limit(
            &self.ledger,
            &self.cfg.calendar,
            hour,
            self.cfg.target,
            self.cfg.safety_margin,
        );
        let input = GuardInput {
            now,
            meter: &snap,
            limit: &limit,
            zones: &self.zones,
            settings: &self.cfg.zone_settings,
            enabled: self.mode != Mode::Off,
        };
        let events = self.guard.update(&self.cfg.guard, &input);
        for g in events {
            out.push(self.event(now, EventKind::Guard { event: g }));
        }

        let decisions = self.decide(now);
        out.extend(self.actuate_all(now, &decisions, self.mode));
        out.push(Output::PublishStatus(Box::new(
            self.status(now, &snap, limit, &decisions),
        )));
        out
    }

    fn decide(&self, now: Timestamp) -> BTreeMap<ZoneId, Decision> {
        let ctx = Context {
            now,
            zones: &self.zones,
            guard: &self.guard,
        };
        let proposals: Vec<Proposal> = self.policies.iter().flat_map(|p| p.propose(&ctx)).collect();
        self.zones
            .values()
            .filter_map(|z| {
                let mine = proposals.iter().filter(|p| &p.zone == z.id());
                arbitrate(&z.config, mine).map(|d| (z.id().clone(), d))
            })
            .collect()
    }

    fn actuate_all(
        &mut self,
        now: Timestamp,
        decisions: &BTreeMap<ZoneId, Decision>,
        mode: Mode,
    ) -> Vec<Output> {
        let mut out = Vec::new();
        for z in self.zones.values_mut() {
            let Some(d) = decisions.get(z.id()) else {
                continue;
            };
            let restore = !self.guard.is_shed(z.id());
            match actuate(&self.cfg.actuator, mode, z, d.value, now, restore) {
                Actuation::None => {}
                Actuation::Write(value) => {
                    z.last_write = Some((value, now));
                    z.awaiting_echo = true;
                    out.push(Output::WriteSetpoint {
                        zone: z.id().clone(),
                        value,
                        reasons: d.reasons.clone(),
                    });
                    out.push(Output::PersistZoneWrite {
                        zone: z.id().clone(),
                        value,
                        at: now,
                    });
                }
                Actuation::WouldWrite(value) => {
                    z.last_would_write = Some((value, now));
                    out.push(Output::WouldWrite {
                        zone: z.id().clone(),
                        value,
                        reasons: d.reasons.clone(),
                    });
                }
            }
        }
        out
    }

    fn status(
        &self,
        now: Timestamp,
        snap: &MeterSnapshot,
        limit: HourLimit,
        decisions: &BTreeMap<ZoneId, Decision>,
    ) -> Status {
        let top_average = self.ledger.top_average();
        let zones = self
            .zones
            .values()
            .map(|z| {
                let d = decisions.get(z.id());
                ZoneStatus {
                    id: z.id().clone(),
                    name: z.config.name.clone(),
                    base: z.base,
                    reported: z.reported,
                    decision: d.map(|d| d.value),
                    reasons: d.map(|d| d.reasons.clone()).unwrap_or_default(),
                    shed: self.guard.is_shed(z.id()),
                    shed_allowed: z.shed_allowed(),
                    override_until: z.active_override(now).map(|o| o.until),
                    power: z.power.map(|(p, _)| p),
                    heating_kw: z.heating_kw,
                }
            })
            .collect();
        Status {
            at: now,
            mode: self.mode,
            guard: self.guard.status,
            hour: snap.hour,
            used: snap.used,
            projected: snap.projected,
            power: snap.power,
            freshness: snap.freshness,
            meter_last_seen: snap.last_seen,
            limit,
            top_days: self.ledger.top_days(),
            top_average,
            tier: self.cfg.tariff.tier_for(top_average),
            zones,
            charger: self.charger.as_ref().map(|c| c.status(now)),
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;
    use jiff::tz::TimeZone;

    use super::*;
    use crate::energy::Quality;
    use crate::tariff::Step;
    use crate::units::Kr;
    use crate::zone::tests::{config, ts};

    const T0: &str = "2026-11-15T17:00:00Z";

    fn cfg() -> EngineConfig {
        EngineConfig {
            calendar: LocalCalendar::new(TimeZone::get("Europe/Oslo").unwrap()),
            tariff: CapacityTariff::new(
                vec![Step {
                    to: Kw::new(5.0),
                    price: Kr::new(233.0),
                }],
                Kr::new(397.0),
            )
            .unwrap(),
            target: Kw::new(5.0),
            safety_margin: Kwh::new(0.3),
            meter: MeterConfig::default(),
            guard: GuardConfig::default(),
            actuator: ActuatorConfig::default(),
            zone_settings: ZoneSettings::default(),
            zones: vec![config("bathroom", 1), config("hallway", 3)],
            charger: None,
        }
    }

    fn id(s: &str) -> ZoneId {
        ZoneId::new(s).unwrap()
    }

    fn engine(state: PersistedState) -> Engine {
        Engine::new(cfg(), state, ts(T0))
    }

    fn c(v: f64) -> Celsius {
        Celsius::new(v)
    }

    #[test]
    fn first_start_is_observe_and_debug_is_compact() {
        let e = engine(PersistedState::default());
        assert_eq!(e.mode(), Mode::Observe);
        let dbg = format!("{e:?}");
        assert!(dbg.starts_with("Engine { mode: Observe, zones: 2"), "{dbg}");
    }

    #[test]
    fn persisted_state_applies_every_persist_output() {
        let mut p = PersistedState::default();
        let hour = HourStart::containing(ts(T0));
        let o = Override {
            value: c(25.0),
            until: ts("2026-11-15T19:00:00Z"),
        };
        let e = HourEnergy {
            hour,
            kwh: Kwh::new(4.0),
            quality: Quality::Measured,
        };
        for out in [
            Output::PersistHour(e),
            Output::PersistZoneWrite {
                zone: id("bathroom"),
                value: c(22.5),
                at: ts(T0),
            },
            Output::PersistZoneBase {
                zone: id("bathroom"),
                value: c(26.5),
            },
            Output::PersistOverride {
                zone: id("bathroom"),
                manual: Some(o),
            },
            Output::PersistZoneStats {
                zone: id("bathroom"),
                heating_kw: Kw::new(1.4),
            },
            Output::PersistMode(Mode::Active),
            Output::PersistShedAllowed {
                zone: id("bathroom"),
                allowed: false,
            },
            // Not persisted state:
            Output::WriteSetpoint {
                zone: id("bathroom"),
                value: c(1.0),
                reasons: vec![],
            },
            Output::WouldWrite {
                zone: id("bathroom"),
                value: c(1.0),
                reasons: vec![],
            },
            Output::PersistZoneHour {
                zone: id("bathroom"),
                hour,
                kwh: Kwh::new(1.0),
            },
            Output::PersistEvent(Event {
                at: ts(T0),
                mode: Mode::Active,
                kind: EventKind::OverrideExpired {
                    zone: id("bathroom"),
                },
            }),
        ] {
            p.apply(&out);
        }
        assert_eq!(p.mode, Some(Mode::Active));
        assert_eq!(p.hours[&hour], e);
        assert_eq!(
            p.zones[&id("bathroom")],
            PersistedZone {
                base: Some(c(26.5)),
                last_write: Some((c(22.5), ts(T0))),
                heating_kw: Some(Kw::new(1.4)),
                shed_allowed: Some(false),
                manual: Some(o),
            }
        );
        // Status outputs are ignored too.
        let mut e = engine(PersistedState::default());
        let tick = e.handle(Input::Tick { at: ts(T0) });
        let before = p.clone();
        for out in &tick {
            p.apply(out);
        }
        assert_eq!(p, before);
    }

    #[test]
    fn zone_inputs_learn_and_persist() {
        let mut e = engine(PersistedState::default());
        let t = ts(T0);
        assert_eq!(
            e.handle(Input::ZonePower {
                zone: id("bathroom"),
                at: t,
                power: Kw::new(1.5),
            }),
            vec![Output::PersistZoneStats {
                zone: id("bathroom"),
                heating_kw: Kw::new(1.5),
            }]
        );
        assert_eq!(
            e.handle(Input::ZonePower {
                zone: id("bathroom"),
                at: t,
                power: Kw::new(0.01),
            }),
            Vec::<Output>::new()
        );
        let energy = |at: &str, total: f64| Input::ZoneEnergy {
            zone: id("bathroom"),
            at: ts(at),
            total: Kwh::new(total),
        };
        assert_eq!(
            e.handle(energy("2026-11-15T17:00:30Z", 100.0)),
            Vec::<Output>::new()
        );
        assert_eq!(
            e.handle(energy("2026-11-15T18:00:30Z", 101.5)),
            vec![Output::PersistZoneHour {
                zone: id("bathroom"),
                hour: HourStart::containing(t),
                kwh: Kwh::new(1.5),
            }]
        );
        assert_eq!(
            e.handle(Input::SetShedAllowed {
                zone: id("hallway"),
                allowed: false,
            }),
            vec![Output::PersistShedAllowed {
                zone: id("hallway"),
                allowed: false,
            }]
        );
    }

    #[test]
    fn inputs_for_unknown_zones_are_ignored() {
        let mut e = engine(PersistedState::default());
        let t = ts(T0);
        let ghost = id("ghost");
        for input in [
            Input::ThermostatSetpoint {
                zone: ghost.clone(),
                at: t,
                value: c(20.0),
            },
            Input::ZonePower {
                zone: ghost.clone(),
                at: t,
                power: Kw::new(1.0),
            },
            Input::ZoneEnergy {
                zone: ghost.clone(),
                at: t,
                total: Kwh::new(1.0),
            },
            Input::SetShedAllowed {
                zone: ghost,
                allowed: true,
            },
        ] {
            assert_eq!(e.handle(input), Vec::<Output>::new());
        }
    }

    #[test]
    fn setting_the_same_mode_does_nothing() {
        let mut e = engine(PersistedState::default());
        assert_eq!(
            e.handle(Input::SetMode {
                at: ts(T0),
                mode: Mode::Observe,
            }),
            Vec::<Output>::new()
        );
    }

    #[test]
    fn entering_active_forgets_observe_mode_simulation() {
        let mut state = PersistedState::default();
        state.zones.insert(
            id("bathroom"),
            PersistedZone {
                base: Some(c(26.5)),
                ..PersistedZone::default()
            },
        );
        let mut e = engine(state);
        let t = ts(T0);
        e.handle(Input::ThermostatSetpoint {
            zone: id("bathroom"),
            at: t,
            value: c(20.0),
        });
        // Observe: base is now 20 (a user change), would-write tracking is per zone.
        e.zones.get_mut(&id("bathroom")).unwrap().last_would_write = Some((c(18.0), t));
        e.handle(Input::SetMode {
            at: t,
            mode: Mode::Active,
        });
        assert_eq!(e.zones[&id("bathroom")].last_would_write, None);
    }

    #[test]
    fn expired_overrides_are_cleared_on_tick() {
        let mut state = PersistedState::default();
        state.zones.insert(
            id("bathroom"),
            PersistedZone {
                base: Some(c(25.0)),
                manual: Some(Override {
                    value: c(25.0),
                    until: ts(T0) - SignedDuration::from_mins(1),
                }),
                ..PersistedZone::default()
            },
        );
        let mut e = engine(state);
        let out = e.handle(Input::Tick { at: ts(T0) });
        assert!(out.contains(&Output::PersistOverride {
            zone: id("bathroom"),
            manual: None,
        }));
        assert!(out.iter().any(|o| matches!(
            o,
            Output::PersistEvent(Event { kind: EventKind::OverrideExpired { zone }, .. }) if *zone == id("bathroom")
        )));
    }

    #[test]
    fn zones_without_a_base_are_left_alone() {
        let mut e = engine(PersistedState::default());
        let out = e.handle(Input::Tick { at: ts(T0) });
        assert!(out.iter().any(|o| matches!(
            o,
            Output::PublishStatus(s)
                if s.zones.iter().all(|z| z.decision.is_none() && z.reasons.is_empty())
        )));
        assert!(!out.iter().any(|o| matches!(o, Output::WouldWrite { .. })));
    }

    #[test]
    fn startup_only_resumes_sheds_in_active_mode() {
        let shed_zone = PersistedZone {
            base: Some(c(26.5)),
            last_write: Some((c(22.5), ts(T0))),
            ..PersistedZone::default()
        };
        let at_base = PersistedZone {
            base: Some(c(22.0)),
            last_write: Some((c(22.0), ts(T0))),
            ..PersistedZone::default()
        };
        let state = |mode| PersistedState {
            mode: Some(mode),
            zones: [
                (id("bathroom"), shed_zone.clone()),
                (id("hallway"), at_base.clone()),
            ]
            .into_iter()
            .collect(),
            ..PersistedState::default()
        };
        let active = engine(state(Mode::Active));
        assert_eq!(active.guard.shed.len(), 1);
        assert_eq!(active.guard.shed[0].zone, id("bathroom"));
        assert_eq!(active.guard.shed[0].since, ts(T0));
        let mut observe = engine(state(Mode::Observe));
        assert_eq!(observe.guard.shed, Vec::<Shed>::new());
        assert!(
            observe.shutdown(ts(T0)).is_empty(),
            "nothing was written in observe"
        );

        // Shutting down in Active restores the shed zone and keeps the mode.
        let mut active = engine(state(Mode::Active));
        let out = active.shutdown(ts(T0));
        assert!(out.contains(&Output::WriteSetpoint {
            zone: id("bathroom"),
            value: c(26.5),
            reasons: vec![crate::policy::Reason {
                policy: crate::policy::PolicyName::UserSetpointBase,
                detail: "user setpoint 26.5 °C".to_owned(),
            }],
        }));
        assert!(!out.iter().any(|o| matches!(o, Output::PersistMode(_))));
        assert_eq!(active.mode(), Mode::Active);
        assert_eq!(active.guard.shed, Vec::<Shed>::new());
    }

    /// Leaving Active right after a shed write, before the thermostat echoed it: its last
    /// report still shows the base, yet the restore must be written (there is no later tick
    /// to fix it).
    #[test]
    fn leaving_active_before_the_echo_still_restores() {
        let mut e = engine(PersistedState {
            mode: Some(Mode::Active),
            ..PersistedState::default()
        });
        let t0 = ts(T0) + SignedDuration::from_mins(30);
        e.handle(Input::ThermostatSetpoint {
            zone: id("bathroom"),
            at: t0,
            value: c(26.5),
        });
        e.handle(Input::ZonePower {
            zone: id("bathroom"),
            at: t0,
            power: Kw::new(1.5),
        });
        let is_write = |o: &Output, v: f64| matches!(o, Output::WriteSetpoint { zone, value, .. } if *zone == id("bathroom") && *value == c(v));
        let mut now = t0;
        while !e
            .handle(Input::Tick { at: now })
            .iter()
            .any(|o| is_write(o, 22.5))
        {
            assert!(now < t0 + SignedDuration::from_mins(5), "never shed");
            now += SignedDuration::from_secs(2);
            e.handle(Input::MeterPower {
                at: now,
                power: Kw::new(9.0),
            });
        }
        let out = e.handle(Input::SetMode {
            at: now + SignedDuration::from_secs(1),
            mode: Mode::Observe,
        });
        assert!(out.iter().any(|o| is_write(o, 26.5)), "{out:#?}");
    }

    #[test]
    fn charger_readings_reach_the_status_only_when_a_charger_is_configured() {
        use crate::charger::{ChargerConfig, ChargerMode, tests::reading};
        let r = reading("2026-11-15T17:00:00Z", ChargerMode::Charging, 2.3);

        let mut without = engine(PersistedState::default());
        assert_eq!(without.handle(Input::Charger(r)), Vec::new());
        let out = without.handle(Input::Tick { at: ts(T0) });
        assert!(
            out.iter()
                .any(|o| matches!(o, Output::PublishStatus(s) if s.charger.is_none()))
        );

        let mut with = Engine::new(
            EngineConfig {
                charger: Some(ChargerConfig::default()),
                ..cfg()
            },
            PersistedState::default(),
            ts(T0),
        );
        assert_eq!(with.handle(Input::Charger(r)), Vec::new());
        let out = with.handle(Input::Tick {
            at: ts("2026-11-15T17:00:10Z"),
        });
        assert!(out.iter().any(|o| matches!(
            o,
            Output::PublishStatus(s)
                if s.charger.is_some_and(|c| c.available && c.reading == Some(r))
        )));
    }
}

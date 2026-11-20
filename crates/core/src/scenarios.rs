//! Replay scenarios (PLAN.md §4) against a closed-loop house simulation.
//!
//! The simulation stands in for the real world: a house with background load plus floor
//! heaters, a meter publishing power every 2 s and the hourly register at each boundary,
//! and thermostats that echo every write and switch their heater off when the setpoint drops
//! below what the floor needs. Snapshots record what the engine did, one line per action.

use std::collections::BTreeMap;

use crate::actuator::ActuatorConfig;
use crate::charger::{ChargerConfig, ChargerMode, ChargerReading};
use crate::energy::HourEnergy;
use crate::energy::{MeterConfig, Quality};
use crate::engine::{Engine, EngineConfig, PersistedState};
use crate::guard::{GuardConfig, GuardEvent};
use crate::ids::ZoneId;
use crate::io::{EventKind, Input, Output, Status};
use crate::mode::Mode;
use crate::tariff::{CapacityTariff, Step};
use crate::time::{HourStart, LocalCalendar};
use crate::units::{Ampere, Celsius, CelsiusDelta, Kr, Kw, Kwh};
use crate::zone::{ZoneConfig, ZoneSettings};
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

const STEP: SignedDuration = SignedDuration::from_secs(2);

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn mins(n: i64) -> SignedDuration {
    SignedDuration::from_mins(n)
}

fn calendar() -> LocalCalendar {
    LocalCalendar::new(TimeZone::get("Europe/Oslo").unwrap())
}

fn tensio() -> CapacityTariff {
    let table = [
        (2.0, 131.0),
        (5.0, 233.0),
        (10.0, 397.0),
        (15.0, 585.0),
        (20.0, 775.0),
        (25.0, 964.0),
        (50.0, 1655.0),
    ];
    let steps = table
        .iter()
        .map(|&(to, price)| Step {
            to: Kw::new(to),
            price: Kr::new(price),
        })
        .collect();
    CapacityTariff::new(steps, Kr::new(2599.0)).unwrap()
}

/// A simulated floor: draws `load` while it `needs_heat` and its setpoint is not lowered.
struct Floor {
    id: ZoneId,
    priority: u8,
    load: f64,
    wanted: f64,
    setpoint: f64,
    needs_heat: bool,
}

impl Floor {
    fn heating(&self) -> bool {
        self.needs_heat && self.setpoint >= self.wanted - 0.1
    }

    fn power(&self) -> f64 {
        if self.heating() { self.load } else { 0.01 }
    }
}

/// The house: Bathroom, Hallway and Basement need heat (3.3 kW); Laundry and the guest unit do not.
fn floors() -> Vec<Floor> {
    let spec = [
        ("bathroom", 1, 1.5, 26.5, true),
        ("laundry", 2, 0.6, 25.0, false),
        ("hallway", 3, 1.0, 22.0, true),
        ("guest_1", 4, 1.0, 18.0, false),
        ("guest_2", 5, 1.0, 18.0, false),
        ("basement", 6, 0.8, 15.0, true),
    ];
    spec.iter()
        .map(|&(id, priority, load, wanted, needs_heat)| Floor {
            id: ZoneId::new(id).unwrap(),
            priority,
            load,
            wanted,
            setpoint: wanted,
            needs_heat,
        })
        .collect()
}

/// A car on the Easee: draws `min(demand, offered) × 230 V` (nothing below 6 A), following
/// a new offer 20 s later. The charger is polled every 30 s.
struct Ev {
    plugged: bool,
    /// What the car would take.
    demand: f64,
    /// The app's "Charger limit".
    app_limit: f64,
    /// The dynamic current and when it expires.
    dynamic: Option<(f64, Timestamp)>,
    power: f64,
    ramp_at: Option<Timestamp>,
    polled: bool,
}

impl Ev {
    fn new() -> Self {
        Self {
            plugged: false,
            demand: 15.0,
            app_limit: 15.0,
            dynamic: None,
            power: 0.0,
            ramp_at: None,
            polled: true,
        }
    }

    fn offered(&self, now: Timestamp) -> f64 {
        let dynamic = self
            .dynamic
            .filter(|&(_, until)| until > now)
            .map_or(f64::MAX, |(a, _)| a);
        self.app_limit.min(dynamic)
    }

    fn wanted_power(&self, now: Timestamp) -> f64 {
        let amps = self.offered(now).min(self.demand);
        if self.plugged && amps >= 6.0 {
            amps * 0.23
        } else {
            0.0
        }
    }

    fn step(&mut self, now: Timestamp) {
        let wanted = self.wanted_power(now);
        if (wanted - self.power).abs() < 1e-9 {
            self.ramp_at = None;
        } else if let Some(at) = self.ramp_at {
            if now >= at {
                self.power = wanted;
                self.ramp_at = None;
            }
        } else {
            self.ramp_at = Some(now + SignedDuration::from_secs(20));
        }
    }

    fn reading(&self, now: Timestamp) -> ChargerReading {
        let mode = match (self.plugged, self.power > 0.0) {
            (false, _) => ChargerMode::Disconnected,
            (true, true) => ChargerMode::Charging,
            (true, false) => ChargerMode::ReadyToCharge,
        };
        ChargerReading {
            at: now,
            mode,
            power: Kw::new(self.power),
            session: Kwh::ZERO,
            dynamic_current: self
                .dynamic
                .filter(|&(_, until)| until > now)
                .map(|(a, _)| Ampere::new(a)),
            max_current: Some(Ampere::new(self.app_limit)),
            offered: Ampere::new(if self.plugged { self.offered(now) } else { 0.0 }),
            connected: true,
        }
    }
}

fn config(floors: &[Floor], charger: Option<ChargerConfig>) -> EngineConfig {
    EngineConfig {
        calendar: calendar(),
        tariff: tensio(),
        target: Kw::new(5.0),
        safety_margin: Kwh::new(0.3),
        meter: MeterConfig::default(),
        guard: GuardConfig::default(),
        actuator: ActuatorConfig::default(),
        zone_settings: ZoneSettings::default(),
        zones: floors
            .iter()
            .map(|f| ZoneConfig {
                id: f.id.clone(),
                name: f.id.as_str().to_owned(),
                priority: f.priority,
                shed_delta: CelsiusDelta::new(4.0),
                min_setpoint: Celsius::new(5.0),
                max_setpoint: Celsius::new(35.0),
                shed_allowed: true,
                nominal_kw: Kw::new(1.0),
            })
            .collect(),
        charger,
    }
}

struct Sim {
    engine: Engine,
    floors: Vec<Floor>,
    ev: Ev,
    charger: Option<ChargerConfig>,
    now: Timestamp,
    background: f64,
    register: f64,
    meter_alive: bool,
    persisted: PersistedState,
    log: Vec<String>,
    outputs: Vec<(Timestamp, Output)>,
    status: Option<Box<Status>>,
}

impl Sim {
    fn new(start: &str, mode: Mode) -> Self {
        Self::with_state(start, mode, PersistedState::default())
    }

    fn with_state(start: &str, mode: Mode, persisted: PersistedState) -> Self {
        Self::build(start, mode, persisted, None)
    }

    /// A house with the EV charger (nothing plugged in yet).
    fn with_charger(start: &str, mode: Mode, charger: ChargerConfig) -> Self {
        Self::build(start, mode, PersistedState::default(), Some(charger))
    }

    fn build(
        start: &str,
        mode: Mode,
        mut persisted: PersistedState,
        charger: Option<ChargerConfig>,
    ) -> Self {
        persisted.mode = Some(mode);
        let now = ts(start);
        let floors = floors();
        let engine = Engine::new(config(&floors, charger), persisted.clone(), now);
        let mut sim = Self {
            engine,
            floors,
            ev: Ev::new(),
            charger,
            now,
            background: 1.0,
            register: 10_000.0,
            meter_alive: true,
            persisted,
            log: Vec::new(),
            outputs: Vec::new(),
            status: None,
        };
        sim.report_thermostats();
        sim
    }

    /// What the thermostats publish (retained) when the service (re)connects.
    fn report_thermostats(&mut self) {
        for i in 0..self.floors.len() {
            let (id, setpoint, power) = {
                let f = &self.floors[i];
                (f.id.clone(), f.setpoint, f.power())
            };
            self.feed(Input::ThermostatSetpoint {
                zone: id.clone(),
                at: self.now,
                value: Celsius::new(setpoint),
            });
            self.feed(Input::ZonePower {
                zone: id,
                at: self.now,
                power: Kw::new(power),
            });
        }
    }

    fn house_power(&self) -> f64 {
        self.background + self.floors.iter().map(Floor::power).sum::<f64>() + self.ev.power
    }

    fn plug_in(&mut self) {
        self.log.push(format!("{} car plugged in", hm(self.now)));
        self.ev.plugged = true;
    }

    fn unplug(&mut self) {
        self.log.push(format!("{} car unplugged", hm(self.now)));
        self.ev.plugged = false;
        self.ev.dynamic = None;
    }

    /// The user sets the app's "Temporary charging limit".
    fn user_limits_charger(&mut self, amps: f64) {
        self.log.push(format!(
            "{} user sets a temporary limit of {amps} A",
            hm(self.now)
        ));
        self.ev.dynamic = Some((amps, self.now + SignedDuration::from_hours(24)));
    }

    fn feed(&mut self, input: Input) {
        let outputs = self.engine.handle(input);
        for o in outputs {
            self.persisted.apply(&o);
            if let Some(line) = describe(self.now, &o) {
                self.log.push(line);
            }
            if let Output::WriteSetpoint { zone, value, .. } = &o {
                self.thermostat_write(zone, value.value());
            }
            if let Output::WriteCharger { write, .. } = &o {
                self.ev.dynamic = Some((write.current.value(), self.now + write.ttl));
            }
            if let Output::PublishStatus(s) = &o {
                self.status = Some(s.clone());
            }
            self.outputs.push((self.now, o));
        }
    }

    /// A thermostat receives a write: it echoes it and reports power if its relay changed.
    fn thermostat_write(&mut self, zone: &ZoneId, value: f64) {
        let f = self.floors.iter_mut().find(|f| &f.id == zone).unwrap();
        let was = f.heating();
        f.setpoint = value;
        let (changed, power) = (was != f.heating(), f.power());
        self.feed(Input::ThermostatSetpoint {
            zone: zone.clone(),
            at: self.now,
            value: Celsius::new(value),
        });
        if changed {
            self.feed(Input::ZonePower {
                zone: zone.clone(),
                at: self.now,
                power: Kw::new(power),
            });
        }
    }

    /// The user changes a setpoint in the Home app.
    fn user_sets(&mut self, zone: &str, value: f64) {
        let id = ZoneId::new(zone).unwrap();
        self.log
            .push(format!("{} user sets {zone} to {value}", hm(self.now)));
        let f = self.floors.iter_mut().find(|f| f.id == id).unwrap();
        f.wanted = value;
        f.setpoint = value;
        let power = f.power();
        self.feed(Input::ThermostatSetpoint {
            zone: id.clone(),
            at: self.now,
            value: Celsius::new(value),
        });
        self.feed(Input::ZonePower {
            zone: id,
            at: self.now,
            power: Kw::new(power),
        });
    }

    fn set_mode(&mut self, mode: Mode) {
        self.feed(Input::SetMode { at: self.now, mode });
    }

    /// Advances the world by `dur`, 2 s at a time.
    fn run(&mut self, dur: SignedDuration) {
        let end = self.now + dur;
        while self.now < end {
            self.register += self.house_power() * STEP.as_secs_f64() / 3600.0;
            self.now += STEP;
            self.ev.step(self.now);
            if HourStart::containing(self.now).start() == self.now {
                self.feed(Input::MeterCumulative {
                    at: self.now,
                    total: Kwh::new(self.register),
                });
            }
            if self.meter_alive {
                self.feed(Input::MeterPower {
                    at: self.now,
                    power: Kw::new(self.house_power()),
                });
            }
            if self.charger.is_some() && self.ev.polled && self.now.as_second() % 30 == 0 {
                self.feed(Input::Charger(self.ev.reading(self.now)));
            }
            if self.now.as_second() % 10 == 0 {
                self.feed(Input::Tick { at: self.now });
            }
        }
    }

    /// Restarts the engine from what the store would hold.
    fn restart(&mut self) {
        self.log.push(format!("{} restart", hm(self.now)));
        self.engine = Engine::new(
            config(&self.floors, self.charger),
            self.persisted.clone(),
            self.now,
        );
        self.report_thermostats();
    }

    fn charger_writes(&self) -> usize {
        self.count(|o| matches!(o, Output::WriteCharger { .. }))
    }

    fn writes(&self) -> usize {
        self.count(|o| matches!(o, Output::WriteSetpoint { .. }))
    }

    fn would_writes(&self) -> usize {
        self.count(|o| matches!(o, Output::WouldWrite { .. }))
    }

    fn sheds(&self) -> usize {
        self.count(|o| {
            matches!(o, Output::PersistEvent(e) if matches!(e.kind, EventKind::Guard { event: GuardEvent::Shed { .. } }))
        })
    }

    fn count_before(&self, t: &str, f: impl Fn(&Output) -> bool) -> usize {
        self.outputs
            .iter()
            .filter(|(at, o)| *at < ts(t) && f(o))
            .count()
    }

    fn count(&self, f: impl Fn(&Output) -> bool) -> usize {
        self.outputs.iter().filter(|(_, o)| f(o)).count()
    }

    fn last_status(&self) -> &Status {
        self.status.as_deref().unwrap()
    }

    fn snapshot(&self) -> String {
        self.log.join("\n")
    }
}

fn hm(t: Timestamp) -> String {
    t.strftime("%m-%d %H:%M:%SZ").to_string()
}

/// One log line per interesting output.
fn describe(now: Timestamp, o: &Output) -> Option<String> {
    let t = hm(now);
    match o {
        Output::WriteSetpoint { zone, value, .. } => Some(format!("{t} write {zone} {value}")),
        Output::WouldWrite { zone, value, .. } => Some(format!("{t} would write {zone} {value}")),
        Output::WriteCharger { write, .. } => Some(format!(
            "{t} write charger {} for {} min",
            write.current,
            write.ttl.as_mins()
        )),
        Output::WouldWriteCharger { write, .. } => Some(format!(
            "{t} would write charger {} for {} min",
            write.current,
            write.ttl.as_mins()
        )),
        Output::PersistEvent(e) => Some(format!(
            "{t} [{}] {}",
            e.mode,
            match &e.kind {
                EventKind::Guard {
                    event:
                        GuardEvent::Shed {
                            zone,
                            load,
                            projected,
                            limit,
                        },
                } => format!("shed {zone} ({load:.2}; projected {projected:.2} > {limit:.2})"),
                EventKind::Guard {
                    event: GuardEvent::Release { zone, reason },
                } => {
                    format!("release {zone} ({reason:?})")
                }
                EventKind::Guard {
                    event:
                        GuardEvent::ChargerLimit {
                            current,
                            saving,
                            projected,
                            limit,
                        },
                } => format!(
                    "limit charger to {current} (saves {saving:.2}; projected {projected:.2}, limit {limit:.2})"
                ),
                EventKind::Guard {
                    event: GuardEvent::ChargerRelease { reason },
                } => format!("release charger ({reason:?})"),
                EventKind::ChargerOverride { current, until } => {
                    format!("charger override at {current} until {}", hm(*until))
                }
                EventKind::Guard {
                    event:
                        GuardEvent::LimitUnreachable {
                            projected,
                            limit,
                            available,
                        },
                } => format!(
                    "limit unreachable (projected {projected:.2}, limit {limit:.2}, sheddable {available:.2})"
                ),
                EventKind::Override { zone, value, until } => {
                    format!("override {zone} at {value} until {}", hm(*until))
                }
                EventKind::OverrideExpired { zone } => format!("override expired {zone}"),
                EventKind::ModeChanged { from, to } => format!("mode {from} -> {to}"),
            }
        )),
        Output::PersistHour(e) if e.quality != Quality::Estimated => Some(format!(
            "{t} hour {} = {:.2} ({:?})",
            hm(e.hour.start()),
            e.kwh,
            e.quality
        )),
        Output::PersistHour(_)
        | Output::PublishStatus(_)
        | Output::PersistZoneWrite { .. }
        | Output::PersistZoneBase { .. }
        | Output::PersistOverride { .. }
        | Output::PersistZoneHour { .. }
        | Output::PersistZoneStats { .. }
        | Output::PersistMode(_)
        | Output::PersistShedAllowed { .. }
        | Output::PersistChargerWrite(_)
        | Output::PersistChargerShedAllowed(_) => None,
    }
}

/// An evening: quiet, then other loads add `extra` kW from `from` for `dur`.
fn spike(mode: Mode, from: i64, dur: i64, extra: f64) -> Sim {
    let mut sim = Sim::new("2026-11-15T17:00:00Z", mode);
    sim.run(mins(from));
    sim.background += extra;
    sim.run(mins(dur));
    sim.background -= extra;
    sim.run(mins(60 - from - dur));
    sim
}

/// The simulated thermostat, like a Z-TRM3, reports power only when a write toggles its relay.
#[test]
fn sim_thermostat_reports_power_only_when_its_relay_changes() {
    let mut sim = Sim::new("2026-11-15T17:00:00Z", Mode::Active);
    let bathroom = ZoneId::new("bathroom").unwrap();
    let reported = |sim: &mut Sim| {
        sim.run(mins(1));
        let status = sim.last_status();
        status
            .zones
            .iter()
            .find(|z| z.id == bathroom)
            .unwrap()
            .power
    };
    // Load changes silently; a write that keeps the relay on reports nothing.
    sim.floors[0].load = 1.6;
    sim.thermostat_write(&bathroom, 27.0);
    assert_eq!(reported(&mut sim), Some(Kw::new(1.5)));
    sim.thermostat_write(&bathroom, 20.0);
    assert_eq!(reported(&mut sim), Some(Kw::new(0.01)));
    sim.thermostat_write(&bathroom, 27.0);
    assert_eq!(reported(&mut sim), Some(Kw::new(1.6)));
}

#[test]
fn s01_quiet_hour_does_nothing() {
    let mut sim = Sim::new("2026-11-15T17:00:00Z", Mode::Active);
    sim.run(mins(60));
    assert_eq!(sim.writes(), 0);
    assert_eq!(sim.sheds(), 0);
    insta::assert_snapshot!(sim.snapshot());
}

#[test]
fn s02_evening_spike_sheds_in_priority_order_and_restores_with_hysteresis() {
    let sim = spike(Mode::Active, 20, 25, 2.5);
    assert!(sim.sheds() >= 1);
    // Bathroom (priority 1) goes first.
    let first = sim.log.iter().find(|l| l.contains("] shed")).unwrap();
    assert!(first.contains("shed bathroom"), "{first}");
    // Everything is back at base by the end of the hour.
    for z in &sim.last_status().zones {
        assert_eq!(z.decision, z.base, "{}", z.id);
        assert!(!z.shed);
    }
    insta::assert_snapshot!(sim.snapshot());
}

#[test]
fn s03_late_spike_needs_less_than_early_spike() {
    let early = spike(Mode::Active, 5, 5, 3.5);
    let late = spike(Mode::Active, 52, 5, 3.5);
    let (e, l) = (early.sheds(), late.sheds());
    assert!(e > l, "early {e} late {l}");
    insta::assert_snapshot!("s03_early", early.snapshot());
    insta::assert_snapshot!("s03_late", late.snapshot());
}

#[test]
fn s04_kettle_shorter_than_the_averaging_window_is_ignored() {
    let mut sim = Sim::new("2026-11-15T17:00:00Z", Mode::Active);
    sim.background = 0.5;
    sim.run(mins(30));
    sim.background += 2.5;
    sim.run(SignedDuration::from_secs(20));
    sim.background -= 2.5;
    sim.run(mins(29) + SignedDuration::from_secs(40));
    assert_eq!(sim.sheds(), 0);
    insta::assert_snapshot!(sim.snapshot());
}

#[test]
fn s05_stale_meter_mid_shed_restores_everything() {
    let mut sim = Sim::new("2026-11-15T17:00:00Z", Mode::Active);
    sim.run(mins(20));
    sim.background += 3.0;
    sim.run(mins(10));
    assert!(sim.sheds() >= 2);
    sim.meter_alive = false;
    sim.run(mins(10));
    for z in &sim.last_status().zones {
        assert!(!z.shed, "{}", z.id);
        assert_eq!(z.decision, z.base);
    }
    insta::assert_snapshot!(sim.snapshot());
}

#[test]
fn s06_user_raising_a_shed_zone_overrides_it_for_two_hours() {
    let mut sim = Sim::new("2026-11-15T17:00:00Z", Mode::Active);
    sim.run(mins(20));
    sim.background += 2.5;
    sim.run(mins(5));
    let bathroom = ZoneId::new("bathroom").unwrap();
    assert!(
        sim.last_status()
            .zones
            .iter()
            .any(|z| z.id == bathroom && z.shed)
    );
    sim.user_sets("bathroom", 27.0);
    sim.run(mins(30));
    let status = sim.last_status();
    let z = status.zones.iter().find(|z| z.id == bathroom).unwrap();
    assert!(!z.shed);
    assert_eq!(z.base, Some(Celsius::new(27.0)));
    assert_eq!(z.override_until, Some(ts("2026-11-15T19:25:00Z")));
    // The load stays high; once the override expires, bathroom is sheddable again.
    sim.run(mins(110));
    let status = sim.last_status();
    let z = status.zones.iter().find(|z| z.id == bathroom).unwrap();
    assert_eq!(z.override_until, None);
    assert!(z.shed, "sheddable again after the override");
    insta::assert_snapshot!(sim.snapshot());
}

#[test]
fn s07_hour_rollover_restores_staggered_without_a_forced_release() {
    let mut sim = Sim::new("2026-11-15T17:00:00Z", Mode::Active);
    sim.run(mins(40));
    sim.background += 2.5;
    sim.run(mins(20));
    sim.background -= 2.5;
    sim.run(mins(20));
    insta::assert_snapshot!(sim.snapshot());
}

#[test]
fn s08_observe_mode_never_writes_but_reports_would_writes() {
    let sim = spike(Mode::Observe, 20, 25, 2.5);
    assert_eq!(sim.writes(), 0);
    assert!(sim.would_writes() > 0);
    insta::assert_snapshot!(sim.snapshot());
    let active = spike(Mode::Active, 20, 25, 2.5);
    assert_eq!(sim.sheds(), active.sheds(), "observe must not cascade");
}

fn hour_record(at: &str, kwh: f64) -> HourEnergy {
    HourEnergy {
        hour: HourStart::containing(ts(at)),
        kwh: Kwh::new(kwh),
        quality: Quality::Measured,
    }
}

fn with_hours(hours: &[(&str, f64)]) -> PersistedState {
    PersistedState {
        hours: hours
            .iter()
            .map(|&(at, kwh)| {
                let e = hour_record(at, kwh);
                (e.hour, e)
            })
            .collect(),
        ..PersistedState::default()
    }
}

#[test]
fn s09_earlier_high_hour_today_relaxes_the_limit() {
    let run = |state: PersistedState| {
        let mut sim = Sim::with_state("2026-11-15T17:00:00Z", Mode::Active, state);
        sim.background += 2.0;
        sim.run(mins(60));
        sim
    };
    let today = run(with_hours(&[("2026-11-15T07:00:00Z", 7.0)]));
    let yesterday = run(with_hours(&[("2026-11-14T17:00:00Z", 7.0)]));
    assert_eq!(today.sheds(), 0, "today already has a 7 kWh hour");
    assert!(yesterday.sheds() > 0, "another day's peak relaxes nothing");
    assert_eq!(
        today.last_status().limit.effective,
        Kwh::new(7.0) - Kwh::new(0.3)
    );
    insta::assert_snapshot!(today.snapshot());
}

#[test]
fn s10_month_rollover_resets_the_ledger() {
    let state = with_hours(&[("2026-11-30T12:00:00Z", 7.0), ("2026-11-29T12:00:00Z", 6.0)]);
    let mut sim = Sim::with_state("2026-11-30T22:50:00Z", Mode::Active, state);
    sim.run(mins(5));
    assert_eq!(sim.last_status().top_days.len(), 2);
    assert_eq!(sim.last_status().limit.allowed, Kwh::new(7.0));
    // 23:00Z is midnight on 1 December locally.
    sim.run(mins(15));
    let s = sim.last_status();
    assert_eq!(s.top_days, Vec::<crate::peaks::DayPeak>::new());
    assert_eq!(s.limit.allowed, Kwh::new(5.0));
    assert_eq!(s.top_average, Kw::ZERO);
    insta::assert_snapshot!(sim.snapshot());
}

/// Runs through a local day and returns its measured hours.
fn local_day(start: &str, day: Date) -> Vec<HourEnergy> {
    let mut sim = Sim::new(start, Mode::Observe);
    sim.run(SignedDuration::from_hours(27));
    let cal = calendar();
    let mut hours: BTreeMap<HourStart, HourEnergy> = BTreeMap::new();
    for (_, o) in &sim.outputs {
        if let Output::PersistHour(e) = o {
            hours.insert(e.hour, *e);
        }
    }
    hours
        .into_values()
        .filter(|e| cal.date_of(e.hour) == day)
        .collect()
}

#[test]
fn s11_dst_days_have_23_and_25_measured_hours() {
    let spring = local_day("2026-03-28T22:00:00Z", Date::new(2026, 3, 29).unwrap());
    assert_eq!(spring.len(), 23);
    assert!(spring.iter().all(|e| e.quality == Quality::Measured));
    let autumn = local_day("2026-10-24T21:00:00Z", Date::new(2026, 10, 25).unwrap());
    assert_eq!(autumn.len(), 25);
    assert!(autumn.iter().all(|e| e.quality == Quality::Measured));
    // Every hour of the quiet house used 4.33 kWh (1 kW background, 3.3 kW of floors and
    // 3 × 10 W standby), including both 02:00 hours.
    assert!(autumn.iter().all(|e| (e.kwh.value() - 4.33).abs() < 1e-6));
}

#[test]
fn s12_restart_mid_shed_continues_and_restores_later() {
    let mut sim = Sim::new("2026-11-15T17:00:00Z", Mode::Active);
    sim.run(mins(20));
    sim.background += 2.5;
    sim.run(mins(5));
    let shed_before: Vec<_> = sim
        .last_status()
        .zones
        .iter()
        .filter(|z| z.shed)
        .map(|z| z.id.clone())
        .collect();
    assert_ne!(shed_before, Vec::<ZoneId>::new());
    let writes_before = sim.writes();
    sim.restart();
    sim.run(mins(1));
    assert_eq!(
        sim.writes(),
        writes_before,
        "no restore-then-reshed flapping"
    );
    let shed_after: Vec<_> = sim
        .last_status()
        .zones
        .iter()
        .filter(|z| z.shed)
        .map(|z| z.id.clone())
        .collect();
    assert_eq!(shed_before, shed_after);
    sim.background -= 2.5;
    sim.run(mins(30));
    assert!(
        sim.last_status()
            .zones
            .iter()
            .all(|z| !z.shed && z.decision == z.base)
    );
    insta::assert_snapshot!(sim.snapshot());
}

#[test]
fn s13_leaving_active_restores_every_shed_zone_at_once() {
    let mut sim = Sim::new("2026-11-15T17:00:00Z", Mode::Active);
    sim.run(mins(20));
    sim.background += 3.0;
    sim.run(mins(5));
    assert!(sim.sheds() >= 2);
    sim.set_mode(Mode::Observe);
    // All floors are back at their own setpoint immediately, then nothing more is written.
    assert!(sim.floors.iter().all(|f| f.setpoint == f.wanted));
    let writes = sim.writes();
    sim.run(mins(10));
    assert_eq!(sim.writes(), writes);
    assert!(
        sim.would_writes() > 0,
        "observe keeps reporting what it would do"
    );
    insta::assert_snapshot!(sim.snapshot());
}

#[test]
fn s14_ev_charging_makes_the_limit_unreachable_and_says_so() {
    let mut sim = Sim::new("2026-11-15T17:00:00Z", Mode::Active);
    sim.run(mins(10));
    // An 11 kW charger: shedding every floor still cannot hold 5 kWh this hour.
    sim.background += 11.0;
    sim.run(mins(30));
    sim.background -= 11.0;
    sim.run(mins(20));
    let unreachable = sim.count(|o| {
        matches!(o, Output::PersistEvent(e) if matches!(e.kind, EventKind::Guard { event: GuardEvent::LimitUnreachable { .. } }))
    });
    assert!(unreachable >= 1);
    insta::assert_snapshot!(sim.snapshot());
}

// ---- EV charger (PLAN.md §9, E2) ----

fn is_charger_limit(o: &Output) -> bool {
    matches!(o, Output::PersistEvent(e) if matches!(e.kind, EventKind::Guard { event: GuardEvent::ChargerLimit { .. } }))
}

fn hour_energy(sim: &Sim, hour: &str) -> f64 {
    sim.outputs
        .iter()
        .rev()
        .find_map(|(_, o)| match o {
            Output::PersistHour(e) if e.hour == HourStart::containing(ts(hour)) => Some(e.kwh),
            _ => None,
        })
        .unwrap()
        .value()
}

fn charger_limit(sim: &Sim) -> Option<f64> {
    sim.last_status()
        .charger
        .unwrap()
        .limit
        .map(|l| l.current().value())
}

/// A car plugged in at 15 A mid-evening: stepping the charger down is enough, and the
/// floors stay warm.
#[test]
fn s15_ev_is_stepped_down_and_the_floors_stay_untouched() {
    let mut sim = Sim::with_charger(
        "2026-11-15T17:00:00Z",
        Mode::Active,
        ChargerConfig::default(),
    );
    sim.background = 0.3;
    sim.run(mins(30));
    sim.plug_in();
    sim.run(mins(31));
    let floor_sheds = sim.count_before("2026-11-15T18:00:00Z", |o| {
        matches!(o, Output::PersistEvent(e) if matches!(e.kind, EventKind::Guard { event: GuardEvent::Shed { .. } }))
    });
    assert_eq!(floor_sheds, 0, "no floor was shed this hour");
    assert!(sim.charger_writes() >= 1);
    assert!(hour_energy(&sim, "2026-11-15T17:00:00Z") <= 5.0);
    insta::assert_snapshot!(sim.snapshot());
}

/// A car at 15 A on top of a cold house: the charger is limited first, and floors are shed
/// only for what it can't cover even at 6 A. After the car is full the floors come back before the charger is released.
#[test]
fn s16_floors_are_shed_only_after_the_charger_reaches_its_floor() {
    let mut sim = Sim::with_charger(
        "2026-11-15T17:00:00Z",
        Mode::Active,
        ChargerConfig::default(),
    );
    sim.plug_in();
    sim.run(mins(30));
    let first_charger = sim
        .outputs
        .iter()
        .position(|(_, o)| is_charger_limit(o))
        .unwrap();
    let first_floor = sim
        .outputs
        .iter()
        .position(|(_, o)| matches!(o, Output::PersistEvent(e) if matches!(e.kind, EventKind::Guard { event: GuardEvent::Shed { .. } })))
        .unwrap();
    assert!(first_charger < first_floor);
    // 6 A plus the floors is still 0.98 kW too much, so a floor has to go; with bathroom
    // shed, 7 A is enough.
    assert_eq!(charger_limit(&sim), Some(7.0));
    sim.ev.demand = 0.0;
    sim.run(mins(40));
    assert!(hour_energy(&sim, "2026-11-15T17:00:00Z") <= 5.0);
    assert_eq!(charger_limit(&sim), None, "released once there is room");
    assert!(sim.last_status().zones.iter().all(|z| !z.shed));
    insta::assert_snapshot!(sim.snapshot());
}

/// The same evening with pausing allowed: the charger is paused instead of shedding floors.
#[test]
fn s17_pausing_when_allowed_spares_the_floors() {
    let run = |pause_allowed| {
        let cfg = ChargerConfig {
            pause_allowed,
            ..ChargerConfig::default()
        };
        let mut sim = Sim::with_charger("2026-11-15T17:00:00Z", Mode::Active, cfg);
        sim.plug_in();
        sim.run(mins(60));
        sim
    };
    let paused = run(true);
    let at_six = run(false);
    assert!(
        paused.sheds() < at_six.sheds(),
        "{} vs {}",
        paused.sheds(),
        at_six.sheds()
    );
    assert!(
        paused
            .log
            .iter()
            .any(|l| l.contains("limit charger to 0 A"))
    );
    assert!(
        !at_six
            .log
            .iter()
            .any(|l| l.contains("limit charger to 0 A"))
    );
    insta::assert_snapshot!(paused.snapshot());
}

/// Easee stops answering: the charger is released by the guard (its limit expires by
/// itself) and the floors carry the load.
#[test]
fn s18_a_stale_charger_leaves_the_floors_to_guard_alone() {
    let mut sim = Sim::with_charger(
        "2026-11-15T17:00:00Z",
        Mode::Active,
        ChargerConfig::default(),
    );
    sim.background = 0.3;
    sim.run(mins(20));
    sim.plug_in();
    sim.run(mins(5));
    assert!(charger_limit(&sim).is_some());
    sim.ev.polled = false;
    // The last reading stays fresh for 3 minutes (a refresh may still go out); after that
    // nothing reaches the charger, and its limit expires by itself.
    sim.run(mins(4));
    let writes = sim.charger_writes();
    sim.run(mins(30));
    assert_eq!(
        sim.charger_writes(),
        writes,
        "nothing is written to an unreachable charger"
    );
    assert!(
        sim.log
            .iter()
            .any(|l| l.contains("release charger (ChargerUnavailable)"))
    );
    assert!(sim.sheds() > 0);
    insta::assert_snapshot!(sim.snapshot());
}

/// Observe mode with the car charging: nothing is written, the charger would be limited,
/// and the floors are not shed more than in active mode.
#[test]
fn s19_observe_mode_with_the_ev_does_not_cascade() {
    let run = |mode| {
        let mut sim = Sim::with_charger("2026-11-15T17:00:00Z", mode, ChargerConfig::default());
        sim.plug_in();
        sim.run(mins(60));
        sim
    };
    let observe = run(Mode::Observe);
    let active = run(Mode::Active);
    assert_eq!(observe.charger_writes() + observe.writes(), 0);
    assert!(observe.count(|o| matches!(o, Output::WouldWriteCharger { .. })) > 0);
    assert!(
        observe.sheds() <= active.sheds(),
        "{} vs {}",
        observe.sheds(),
        active.sheds()
    );
    insta::assert_snapshot!(observe.snapshot());
}

/// Unplugging clears the charger's limit by itself: no release write, no retries.
#[test]
fn s20_unplugging_a_limited_car_needs_no_write() {
    let mut sim = Sim::with_charger(
        "2026-11-15T17:00:00Z",
        Mode::Active,
        ChargerConfig::default(),
    );
    sim.plug_in();
    sim.run(mins(10));
    assert!(charger_limit(&sim).is_some());
    sim.unplug();
    let writes = sim.charger_writes();
    sim.run(mins(30));
    assert_eq!(sim.charger_writes(), writes);
    assert_eq!(charger_limit(&sim), None);
    assert_eq!(sim.persisted.charger.last_write, None);
    insta::assert_snapshot!(sim.snapshot());
}

/// The user sets a temporary limit in the app while the guard limits the charger: the guard
/// leaves the charger alone (no write over the user's limit) and sheds floors instead.
#[test]
fn s21_a_temporary_limit_from_the_app_is_an_override() {
    let mut sim = Sim::with_charger(
        "2026-11-15T17:00:00Z",
        Mode::Active,
        ChargerConfig::default(),
    );
    sim.plug_in();
    sim.run(mins(10));
    sim.user_limits_charger(12.0);
    let writes = sim.charger_writes();
    sim.run(mins(30));
    assert_eq!(sim.charger_writes(), writes);
    let status = sim.last_status().charger.unwrap();
    assert_eq!(status.limit, None);
    assert_eq!(status.override_until, Some(ts("2026-11-15T19:10:30Z")));
    assert!(sim.sheds() > 0);
    insta::assert_snapshot!(sim.snapshot());
}

/// Leaving active mode releases the charger at once; a restart mid-limit keeps it.
#[test]
fn s22_restart_keeps_and_leaving_active_releases_the_charger_limit() {
    let mut sim = Sim::with_charger(
        "2026-11-15T17:00:00Z",
        Mode::Active,
        ChargerConfig::default(),
    );
    sim.plug_in();
    sim.run(mins(10));
    let limited = charger_limit(&sim);
    assert!(limited.is_some());
    let writes = sim.charger_writes();
    sim.restart();
    sim.run(mins(1));
    assert_eq!(
        sim.charger_writes(),
        writes,
        "no release-then-limit flapping"
    );
    assert_eq!(charger_limit(&sim), limited);
    sim.set_mode(Mode::Observe);
    assert_eq!(sim.charger_writes(), writes + 1);
    assert!(sim.log.last().unwrap().contains("mode active -> observe"));
    assert!(
        sim.ev.offered(sim.now) >= 15.0,
        "back to the app's own limit"
    );
    insta::assert_snapshot!(sim.snapshot());
}

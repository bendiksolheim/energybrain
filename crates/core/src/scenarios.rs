//! Replay scenarios (PLAN.md §4) against a closed-loop house simulation.
//!
//! The simulation stands in for the real world: a house with background load plus floor
//! heaters, a meter publishing power every 2 s and the hourly register at each boundary,
//! and thermostats that echo every write and switch their heater off when the setpoint drops
//! below what the floor needs. Snapshots record what the engine did, one line per action.

use std::collections::BTreeMap;

use crate::actuator::ActuatorConfig;
use crate::energy::HourEnergy;
use crate::energy::{MeterConfig, Quality};
use crate::engine::{Engine, EngineConfig, PersistedState};
use crate::guard::{GuardConfig, GuardEvent};
use crate::ids::ZoneId;
use crate::io::{EventKind, Input, Output, Status};
use crate::mode::Mode;
use crate::tariff::{CapacityTariff, Step};
use crate::time::{HourStart, LocalCalendar};
use crate::units::{Celsius, CelsiusDelta, Kr, Kw, Kwh};
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

/// The house: Bad, Gang and Kjeller need heat (3.3 kW); Vaskerom and the rental do not.
fn floors() -> Vec<Floor> {
    let spec = [
        ("bad", 1, 1.5, 26.5, true),
        ("vaskerom", 2, 0.6, 25.0, false),
        ("gang", 3, 1.0, 22.0, true),
        ("utleie_1", 4, 1.0, 18.0, false),
        ("utleie_2", 5, 1.0, 18.0, false),
        ("kjeller", 6, 0.8, 15.0, true),
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

fn config(floors: &[Floor]) -> EngineConfig {
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
    }
}

struct Sim {
    engine: Engine,
    floors: Vec<Floor>,
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

    fn with_state(start: &str, mode: Mode, mut persisted: PersistedState) -> Self {
        persisted.mode = Some(mode);
        let now = ts(start);
        let floors = floors();
        let engine = Engine::new(config(&floors), persisted.clone(), now);
        let mut sim = Self {
            engine,
            floors,
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
        self.background + self.floors.iter().map(Floor::power).sum::<f64>()
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
            if self.now.as_second() % 10 == 0 {
                self.feed(Input::Tick { at: self.now });
            }
        }
    }

    /// Restarts the engine from what the store would hold.
    fn restart(&mut self) {
        self.log.push(format!("{} restart", hm(self.now)));
        self.engine = Engine::new(config(&self.floors), self.persisted.clone(), self.now);
        self.report_thermostats();
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
        | Output::PersistShedAllowed { .. } => None,
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
    // Bad (priority 1) goes first.
    let first = sim.log.iter().find(|l| l.contains("] shed")).unwrap();
    assert!(first.contains("shed bad"), "{first}");
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
    let bad = ZoneId::new("bad").unwrap();
    assert!(
        sim.last_status()
            .zones
            .iter()
            .any(|z| z.id == bad && z.shed)
    );
    sim.user_sets("bad", 27.0);
    sim.run(mins(30));
    let status = sim.last_status();
    let z = status.zones.iter().find(|z| z.id == bad).unwrap();
    assert!(!z.shed);
    assert_eq!(z.base, Some(Celsius::new(27.0)));
    assert_eq!(z.override_until, Some(ts("2026-11-15T19:25:00Z")));
    // The load stays high; once the override expires, bad is sheddable again.
    sim.run(mins(110));
    let status = sim.last_status();
    let z = status.zones.iter().find(|z| z.id == bad).unwrap();
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
    assert!(s.top_days.is_empty());
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
    assert!(!shed_before.is_empty());
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

//! The simulated house on the wire: a Kaifa meter behind a Tibber Pulse, one Z-TRM3 per
//! floor, and `sim/…` control topics.
//!
//! [`House`] is a pure discrete-event simulation. [`House::step`] runs it up to a given
//! simulated instant and returns what the devices published on the way, each at its own
//! moment; physics is integrated piecewise between those moments. Time only comes in as an
//! argument, so the same run can be driven by a test or by an accelerated clock.

use energybrain_core::time::HourStart;
use energybrain_core::units::{Ampere, Celsius, CelsiusDelta, Kvar, Kvarh, Kw, Kwh, Volt};
use energybrain_han::{Details, MeterTime, Phases, Reading, Register, encode};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde_json::json;

use crate::Publish;
use crate::config::SimConfig;
use crate::floor::Floor;
use crate::loads::Loads;
use crate::rng::Rng;

/// How far a floor's environment leans from the indoor towards the outdoor temperature.
pub const OUTDOOR_SHARE: f64 = 0.2;
/// Retained JSON snapshot of the simulation, republished every simulated minute.
pub const STATE_TOPIC: &str = "sim/state";

const LIST1_EVERY: SignedDuration = SignedDuration::from_millis(2500);
const LIST2_EVERY: SignedDuration = SignedDuration::from_secs(10);
const POWER_EVERY: SignedDuration = SignedDuration::from_secs(90);
const ENERGY_EVERY: SignedDuration = SignedDuration::from_mins(5);
const PULSE_STATUS_EVERY: SignedDuration = SignedDuration::from_mins(10);
const MINUTE: SignedDuration = SignedDuration::from_mins(1);
/// A Z-TRM3 reports a written setpoint back twice within a second.
const ECHOES: [SignedDuration; 2] = [
    SignedDuration::from_millis(300),
    SignedDuration::from_millis(800),
];
/// The house's meter (with the serial anonymised, as in the test fixtures).
const LIST_ID: &str = "KFM_001";
const METER_ID: &str = "1234567890123456";
const METER_TYPE: &str = "MA304H3E";
/// Line-to-line voltage of the IT net (UL2 reads 0 there).
const VOLTAGE: f64 = 230.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Capture,
    Minute,
    List2,
    List1,
    PulseStatus,
    Power(usize),
    Energy(usize),
    Queued(usize),
}

/// The simulated house.
#[derive(Debug, Clone)]
pub struct House {
    tz: TimeZone,
    meter_topic: String,
    now: Timestamp,
    rng: Rng,
    indoor: Celsius,
    floors: Vec<Floor>,
    loads: Loads,
    register: Kwh,
    captured: Kwh,
    meter_paused: bool,
    next_capture: Timestamp,
    next_minute: Timestamp,
    next_list2: Timestamp,
    next_list1: Timestamp,
    next_pulse: Timestamp,
    next_power: Vec<Timestamp>,
    next_energy: Vec<Timestamp>,
    queue: Vec<(Timestamp, Publish)>,
}

impl House {
    /// The house at `cfg.start`, and what it publishes on startup: every thermostat's
    /// retained values.
    pub fn new(cfg: &SimConfig) -> (Self, Vec<Publish>) {
        let mut rng = Rng::new(cfg.seed);
        let floors: Vec<Floor> = cfg
            .floors
            .iter()
            .map(|spec| Floor::new(spec.clone(), CelsiusDelta::new(0.5 * rng.symmetric())))
            .collect();
        let start = cfg.start;
        let staggered = |first: i64, gap: i64| -> Vec<Timestamp> {
            (0..floors.len())
                .map(|i| {
                    start + SignedDuration::from_secs(first + gap * i64::try_from(i).unwrap_or(0))
                })
                .collect()
        };
        let next_power = staggered(3, 7);
        let next_energy = staggered(60, 11);
        let house = Self {
            tz: cfg.tz.clone(),
            meter_topic: cfg.meter_topic.clone(),
            now: start,
            rng,
            indoor: cfg.indoor,
            loads: Loads::new(
                cfg.tz.clone(),
                cfg.background.clone(),
                cfg.heat_pump,
                cfg.appliances.clone(),
                cfg.ev.clone(),
                cfg.outdoor,
            ),
            floors,
            register: cfg.register,
            captured: cfg.register,
            meter_paused: false,
            next_capture: HourStart::containing(start).next().start(),
            next_minute: start,
            next_list2: aligned(start, LIST2_EVERY),
            next_list1: start,
            next_pulse: start + PULSE_STATUS_EVERY / 2,
            next_power,
            next_energy,
            queue: Vec::new(),
        };
        let out = house.retained();
        (house, out)
    }

    /// Every thermostat's current setpoint, power and energy, retained: what a broker that
    /// lost its state needs to be told again.
    pub fn retained(&self) -> Vec<Publish> {
        (0..self.floors.len())
            .flat_map(|i| {
                [
                    self.setpoint_report(i),
                    self.power_report(i),
                    self.energy_report(i),
                ]
            })
            .collect()
    }

    /// The simulated instant the house has reached.
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// Whole-house power, as the meter measures it.
    pub fn power(&self) -> Kw {
        self.loads.breakdown(self.now).total() + self.floors_power()
    }

    /// The floors.
    pub fn floors(&self) -> &[Floor] {
        &self.floors
    }

    /// One log line: local time, whole-house power by load, and the floors heating.
    pub fn summary(&self) -> String {
        let b = self.loads.breakdown(self.now);
        let heating: Vec<&str> = self
            .floors
            .iter()
            .filter(|f| f.heating())
            .map(|f| f.spec().topics.zone.as_str())
            .collect();
        format!(
            "{} {:.2} kW = floors {:.2} + background {:.2} + heat pump {:.2} + appliances {:.2} \
             + ev {:.2} + extra {:.2}; heating: {}{}",
            self.tz.to_datetime(self.now).strftime("%a %H:%M"),
            self.power().value(),
            self.floors_power().value(),
            b.background.value(),
            b.heat_pump.value(),
            b.appliances.value(),
            b.ev.value(),
            b.extra.value(),
            if heating.is_empty() {
                "none".to_owned()
            } else {
                heating.join(", ")
            },
            if self.meter_paused {
                " (meter paused)"
            } else {
                ""
            },
        )
    }

    /// Topic filters the house must subscribe to: setpoint writes and the control topics.
    pub fn subscriptions(&self) -> Vec<String> {
        let mut subs: Vec<String> = self
            .floors
            .iter()
            .map(|f| format!("{}/set", f.spec().topics.setpoint))
            .collect();
        subs.push("sim/+/set".to_owned());
        subs.push("sim/zone/+/setpoint/set".to_owned());
        subs
    }

    /// Runs the house up to `to` and returns everything published on the way, in order.
    /// An instant before [`House::now`] changes nothing.
    pub fn step(&mut self, to: Timestamp) -> Vec<Publish> {
        let mut out = Vec::new();
        loop {
            let (at, event) = self.next_event();
            if at > to {
                break;
            }
            self.integrate(at);
            self.handle(event, &mut out);
        }
        self.integrate(to);
        out
    }

    /// Handles a message on one of [`House::subscriptions`] at [`House::now`] (step the
    /// house to the current instant first). Unknown topics are ignored; a bad control
    /// payload is an error describing it.
    pub fn on_message(&mut self, topic: &str, payload: &[u8]) -> Result<Vec<Publish>, String> {
        let text = String::from_utf8_lossy(payload);
        let text = text.trim();
        if let Some(i) = self
            .floors
            .iter()
            .position(|f| topic.strip_suffix("/set") == Some(f.spec().topics.setpoint.as_str()))
        {
            self.floors[i].set_setpoint(Celsius::new(number(text)?));
            let echo = self.setpoint_report(i);
            for delay in ECHOES {
                self.queue.push((self.now + delay, echo.clone()));
            }
            return Ok(Vec::new());
        }
        let Some(control) = topic
            .strip_prefix("sim/")
            .and_then(|r| r.strip_suffix("/set"))
        else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        match control.split('/').collect::<Vec<_>>().as_slice() {
            ["ev"] if text.eq_ignore_ascii_case("off") => self.loads.set_ev(None),
            ["ev"] => self.loads.set_ev(Some(Kw::new(positive(text)?))),
            ["load"] => self.loads.set_extra(Kw::new(non_negative(text)?)),
            ["meter"] => {
                self.meter_paused = match text.to_ascii_lowercase().as_str() {
                    "pause" => true,
                    "resume" => false,
                    _ => return Err(format!("expected pause or resume, got {text:?}")),
                }
            }
            ["outdoor"] => self.loads.set_outdoor(Celsius::new(number(text)?)),
            ["zone", id, "setpoint"] => {
                let i = self
                    .floors
                    .iter()
                    .position(|f| f.spec().topics.zone.as_str() == *id)
                    .ok_or_else(|| format!("unknown zone {id:?}"))?;
                self.floors[i].set_setpoint(Celsius::new(number(text)?));
                out.push(self.setpoint_report(i));
            }
            _ => return Err(format!("unknown control topic {topic:?}")),
        }
        out.push(self.state());
        Ok(out)
    }

    fn next_event(&self) -> (Timestamp, Event) {
        let mut best = (self.next_capture, Event::Capture);
        let mut consider = |at: Timestamp, event: Event| {
            if at < best.0 {
                best = (at, event);
            }
        };
        consider(self.next_minute, Event::Minute);
        consider(self.next_list2, Event::List2);
        consider(self.next_list1, Event::List1);
        consider(self.next_pulse, Event::PulseStatus);
        for (i, &at) in self.next_power.iter().enumerate() {
            consider(at, Event::Power(i));
        }
        for (i, &at) in self.next_energy.iter().enumerate() {
            consider(at, Event::Energy(i));
        }
        for (i, (at, _)) in self.queue.iter().enumerate() {
            consider(*at, Event::Queued(i));
        }
        best
    }

    /// Lets time pass until `to` with the loads as they are now.
    fn integrate(&mut self, to: Timestamp) {
        let dt = to.duration_since(self.now);
        if !dt.is_positive() {
            return;
        }
        self.register += self.power() * dt;
        let environment = self.indoor + (self.loads.outdoor() - self.indoor) * OUTDOOR_SHARE;
        for (i, floor) in self.floors.iter_mut().enumerate() {
            if floor.advance(dt, environment) {
                // A Z-TRM3 reports power as soon as its heater switches.
                self.next_power[i] = to;
            }
        }
        self.loads.advance(dt, to);
        self.now = to;
    }

    fn handle(&mut self, event: Event, out: &mut Vec<Publish>) {
        let now = self.now;
        match event {
            Event::Capture => {
                // List 3 carries the registers as they were at the hour shift.
                self.captured = self.register;
                self.next_capture += SignedDuration::from_hours(1);
            }
            Event::Minute => {
                self.loads.minute(now, &mut self.rng);
                out.push(self.state());
                self.next_minute += MINUTE;
            }
            Event::List2 => {
                let local = self.tz.to_datetime(now);
                let list3 = local.minute() == 0 && local.second() == 10;
                self.meter(out, true, list3);
                self.next_list2 += LIST2_EVERY;
            }
            Event::List1 => {
                self.meter(out, false, false);
                self.next_list1 += LIST1_EVERY;
            }
            Event::PulseStatus => {
                if !self.meter_paused {
                    let status =
                        json!({ "status": { "Build": "sim", "ssid": "sim", "rssi": -55 } });
                    out.push(Publish::text(
                        self.meter_topic.clone(),
                        status.to_string(),
                        false,
                    ));
                }
                self.next_pulse += PULSE_STATUS_EVERY;
            }
            Event::Power(i) => {
                out.push(self.power_report(i));
                self.next_power[i] = now + POWER_EVERY;
            }
            Event::Energy(i) => {
                out.push(self.energy_report(i));
                self.next_energy[i] = now + ENERGY_EVERY;
            }
            Event::Queued(i) => out.push(self.queue.remove(i).1),
        }
    }

    /// Publishes one HAN frame unless the meter is paused.
    fn meter(&self, out: &mut Vec<Publish>, details: bool, register: bool) {
        if self.meter_paused {
            return;
        }
        let power = self.power();
        let clock = self.meter_time();
        let current = Ampere::new(power.value() * 1000.0 / (3f64.sqrt() * VOLTAGE));
        let reading = Reading {
            sent: Some(clock),
            import_power: power,
            details: details.then(|| Details {
                list_id: LIST_ID.to_owned(),
                meter_id: METER_ID.to_owned(),
                meter_type: METER_TYPE.to_owned(),
                export_power: Kw::ZERO,
                reactive_import: Kvar::ZERO,
                reactive_export: Kvar::new(0.2),
                phases: Phases::Three {
                    currents: [current; 3],
                    voltages: [Volt::new(VOLTAGE), Volt::ZERO, Volt::new(VOLTAGE)],
                },
            }),
            register: register.then_some(Register {
                clock,
                import: self.captured,
                export: Kwh::ZERO,
                reactive_import: Kvarh::ZERO,
                reactive_export: Kvarh::ZERO,
            }),
        };
        // Our texts are short constants and a register always comes with details, which
        // are the only two things `encode` rejects.
        #[allow(clippy::expect_used)]
        let frame = encode(&reading).expect("simulated readings are always encodable");
        out.push(Publish {
            topic: self.meter_topic.clone(),
            payload: frame,
            retain: false,
        });
    }

    /// The meter's clock: local wall time to the second, deviation unspecified, DST bit set
    /// in summer, as a Kaifa meter sends it.
    fn meter_time(&self) -> MeterTime {
        let whole = self.now - SignedDuration::from_nanos(i64::from(self.now.subsec_nanosecond()));
        MeterTime {
            datetime: self.tz.to_datetime(whole),
            deviation: None,
            dst: Some(self.tz.to_offset_info(self.now).dst().is_dst()),
        }
    }

    fn floors_power(&self) -> Kw {
        self.floors.iter().map(Floor::power).sum()
    }

    fn setpoint_report(&self, i: usize) -> Publish {
        let f = &self.floors[i];
        Publish::text(
            f.spec().topics.setpoint.clone(),
            f.setpoint().value().to_string(),
            true,
        )
    }

    fn power_report(&self, i: usize) -> Publish {
        let f = &self.floors[i];
        let watts = f.power().value() * 1000.0;
        Publish::text(f.spec().topics.power.clone(), format!("{watts:.1}"), true)
    }

    fn energy_report(&self, i: usize) -> Publish {
        let f = &self.floors[i];
        let kwh = f.energy().value();
        Publish::text(f.spec().topics.energy.clone(), format!("{kwh:.2}"), true)
    }

    /// The retained `sim/state` snapshot. Quantities use the unit types, so they serialize
    /// as `{"value": …, "unit": …}` like energybrain's own API.
    fn state(&self) -> Publish {
        let b = self.loads.breakdown(self.now);
        let kw = |v: Kw| Kw::new(round(v.value(), 3));
        let floors: Vec<_> = self
            .floors
            .iter()
            .map(|f| {
                json!({
                    "zone": f.spec().topics.zone.as_str(),
                    "temperature": Celsius::new(round(f.temperature().value(), 2)),
                    "setpoint": f.setpoint(),
                    "heating": f.heating(),
                })
            })
            .collect();
        let state = json!({
            "at": self.now.to_string(),
            "power": kw(self.power()),
            "loads": {
                "background": kw(b.background),
                "heat_pump": kw(b.heat_pump),
                "appliances": kw(b.appliances),
                "ev": kw(b.ev),
                "extra": kw(b.extra),
                "floors": kw(self.floors_power()),
            },
            "running": self.loads.running(),
            "outdoor": self.loads.outdoor(),
            "meter_paused": self.meter_paused,
            "register": Kwh::new(round(self.register.value(), 3)),
            "floors": floors,
        });
        Publish::text(STATE_TOPIC.to_owned(), state.to_string(), true)
    }
}

/// The first multiple of `every` (counted from the Unix epoch) at or after `t`.
fn aligned(t: Timestamp, every: SignedDuration) -> Timestamp {
    let period = every.as_millis();
    let rem = i128::from(t.as_millisecond()).rem_euclid(period);
    let wait = i64::try_from((period - rem) % period).unwrap_or(0);
    t + SignedDuration::from_millis(wait)
}

fn round(v: f64, decimals: i32) -> f64 {
    let scale = 10f64.powi(decimals);
    (v * scale).round() / scale
}

fn number(text: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| format!("not a number: {text:?}"))
}

fn non_negative(text: &str) -> Result<f64, String> {
    let v = number(text)?;
    if v >= 0.0 {
        Ok(v)
    } else {
        Err(format!("{v} is negative"))
    }
}

fn positive(text: &str) -> Result<f64, String> {
    let v = number(text)?;
    if v > 0.0 {
        Ok(v)
    } else {
        Err(format!("{v} must be above 0"))
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use energybrain_core::io::Input;
    use energybrain_mqtt::translate::{Decoded, Translator};
    use serde_json::Value;

    use super::*;
    use crate::config::tests::{MINIMAL, eb};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn house(extra: &str) -> (House, Vec<Publish>) {
        let cfg = crate::parse(&format!("{MINIMAL}\n{extra}"), &eb()).unwrap();
        House::new(&cfg)
    }

    fn secs(n: i64) -> SignedDuration {
        SignedDuration::from_secs(n)
    }

    fn text(p: &Publish) -> &str {
        std::str::from_utf8(&p.payload).unwrap()
    }

    fn on(out: &[Publish], topic: &str) -> Vec<String> {
        out.iter()
            .filter(|p| p.topic == topic)
            .map(|p| text(p).to_owned())
            .collect()
    }

    fn count(out: &[Publish], topic: &str) -> usize {
        out.iter().filter(|p| p.topic == topic).count()
    }

    fn state(out: &[Publish]) -> Value {
        let last = out.iter().rev().find(|p| p.topic == STATE_TOPIC).unwrap();
        serde_json::from_slice(&last.payload).unwrap()
    }

    const BAD_SETPOINT: &str = "zwave/Bad/Gulv_bad/67/1/setpoint/1";
    const BAD_POWER: &str = "zwave/Bad/Gulv_bad/50/1/value/66049";

    #[test]
    fn startup_publishes_retained_thermostat_values_and_the_mode() {
        let (h, out) = house("");
        assert_eq!(h.now(), ts("2026-12-01T15:00:00Z"));
        assert_eq!(out.len(), 6 * 3);
        assert_eq!(out[0], Publish::text(BAD_SETPOINT.into(), "22", true));
        assert_eq!(out[1].topic, BAD_POWER);
        assert!(["1000.0", "0.1"].contains(&text(&out[1])));
        assert_eq!(text(&out[2]), "0.00");
        assert!(out.iter().all(|p| p.retain));
        assert_eq!(h.floors().len(), 6);
        let subs = h.subscriptions();
        assert_eq!(subs[0], format!("{BAD_SETPOINT}/set"));
        assert_eq!(&subs[6..], ["sim/+/set", "sim/zone/+/setpoint/set"]);
    }

    #[test]
    fn an_hour_on_the_wire_is_what_energybrain_expects() {
        let config = eb();
        let tr = Translator::new(&config.topics, config.engine.calendar.time_zone().clone());
        let (mut h, out) = house("");
        let mut decoded: Vec<Decoded> = out
            .iter()
            .map(|p| tr.decode(&p.topic, &p.payload, h.now()))
            .collect();
        // Step like the driver would: small, uneven real-time slices.
        let end = h.now() + SignedDuration::from_mins(61);
        while h.now() < end {
            let to = h.now() + SignedDuration::from_millis(700);
            for p in h.step(to) {
                decoded.push(tr.decode(&p.topic, &p.payload, h.now()));
            }
        }
        let invalid: Vec<_> = decoded
            .iter()
            .filter(|d| matches!(d, Decoded::Invalid { .. }))
            .collect();
        assert_eq!(invalid, Vec::<&Decoded>::new());
        let ignored = decoded
            .iter()
            .filter(|d| matches!(d, Decoded::Ignored))
            .count();
        let inputs: Vec<&Input> = decoded
            .iter()
            .filter_map(|d| match d {
                Decoded::Inputs(i) => Some(i),
                Decoded::Ignored | Decoded::Invalid { .. } => None,
            })
            .flatten()
            .collect();
        let meter_power = inputs
            .iter()
            .filter(|i| matches!(i, Input::MeterPower { .. }))
            .count();
        let registers: Vec<(Timestamp, Kwh)> = inputs
            .iter()
            .filter_map(|i| {
                if let Input::MeterCumulative { at, total } = i {
                    Some((*at, *total))
                } else {
                    None
                }
            })
            .collect();
        // List 1 every 2.5 s plus list 2 every 10 s, for 61 minutes.
        assert!((1820..=1835).contains(&meter_power), "{meter_power}");
        // The run starts on the hour, so the first list 3 (at 16:00:10 local) carries the
        // starting register; the next one, captured at the following hour shift, has the
        // hour's energy in it.
        assert_eq!(
            registers[0],
            (ts("2026-12-01T15:00:00Z"), Kwh::new(10_000.0))
        );
        assert_eq!(registers.len(), 2);
        let (at, total) = registers[1];
        assert_eq!(at, ts("2026-12-01T16:00:00Z"));
        assert!(
            total > Kwh::new(10_002.0) && total < Kwh::new(10_010.0),
            "{total}"
        );
        // sim/state every minute and the Pulse's JSON status are ignored by energybrain.
        assert!(ignored >= 61 + 6, "{ignored}");
    }

    #[test]
    fn setpoint_writes_are_echoed_twice_within_a_second() {
        let (mut h, _) = house("");
        h.step(h.now() + secs(5));
        let write = format!("{BAD_SETPOINT}/set");
        assert_eq!(h.on_message(&write, b"21.7"), Ok(vec![]));
        let out = h.step(h.now() + secs(1));
        assert_eq!(on(&out, BAD_SETPOINT), ["21.5", "21.5"]);
        assert_eq!(h.floors()[0].setpoint(), Celsius::new(21.5));
        assert_eq!(
            h.on_message(&write, b"warm"),
            Err("not a number: \"warm\"".into())
        );
        assert_eq!(
            h.on_message(&write, b"inf"),
            Err("not a number: \"inf\"".into())
        );
    }

    #[test]
    fn heater_switching_is_reported_at_once_and_otherwise_every_90_seconds() {
        let (mut h, _) = house(
            "[[floor]]\nzone = \"bad\"\nsetpoint = 26\n[[floor]]\nzone = \"gang\"\nsetpoint = 26",
        );
        let out = h.step(h.now() + SignedDuration::from_mins(10));
        let reports = on(&out, BAD_POWER).len();
        assert!((6..=8).contains(&reports), "{reports}");
        // Make sure the floor heats, then shed it: the heater stops within a second.
        h.on_message(&format!("{BAD_SETPOINT}/set"), b"30").unwrap();
        h.step(h.now() + secs(1));
        assert!(h.floors()[0].heating());
        h.on_message(&format!("{BAD_SETPOINT}/set"), b"22").unwrap();
        let out = h.step(h.now() + secs(1));
        assert_eq!(on(&out, BAD_POWER), ["0.1"]);
        assert!(!h.floors()[0].heating());
    }

    #[test]
    fn energy_counters_are_reported_every_five_minutes() {
        let (mut h, _) = house("");
        let out = h.step(h.now() + SignedDuration::from_mins(30));
        let counts = on(&out, "zwave/Bad/Gulv_bad/50/1/value/65537");
        assert_eq!(counts.len(), 6);
        let values: Vec<f64> = counts.iter().map(|c| c.parse().unwrap()).collect();
        assert!(values.windows(2).all(|w| w[0] <= w[1]), "{values:?}");
    }

    #[test]
    fn ev_and_extra_load_show_up_on_the_meter() {
        let (mut h, _) = house("");
        h.step(h.now() + secs(1));
        let before = h.power();
        let out = h.on_message("sim/ev/set", b"7.4").unwrap();
        assert_eq!(state(&out)["loads"]["ev"]["value"], 7.4);
        assert!((h.power() - before - Kw::new(7.4)).value().abs() < 1e-9);
        h.on_message("sim/ev/set", b"OFF").unwrap();
        let out = h.on_message("sim/load/set", b"2").unwrap();
        assert_eq!(state(&out)["loads"]["extra"]["value"], 2.0);
        assert_eq!(state(&out)["loads"]["ev"]["value"], 0.0);
        assert_eq!(
            h.on_message("sim/ev/set", b"0"),
            Err("0 must be above 0".into())
        );
        assert_eq!(
            h.on_message("sim/load/set", b"-1"),
            Err("-1 is negative".into())
        );
        assert_eq!(
            h.on_message("sim/load/set", b"lots"),
            Err("not a number: \"lots\"".into())
        );
        assert_eq!(
            h.on_message("sim/ev/set", b"fast"),
            Err("not a number: \"fast\"".into())
        );
    }

    #[test]
    fn a_paused_meter_publishes_nothing_until_resumed() {
        let (mut h, _) = house("");
        let out = h.on_message("sim/meter/set", b"pause").unwrap();
        assert_eq!(state(&out)["meter_paused"], true);
        let out = h.step(h.now() + SignedDuration::from_mins(10));
        assert_eq!(count(&out, "tibber"), 0);
        h.on_message("sim/meter/set", b"Resume").unwrap();
        let out = h.step(h.now() + secs(10));
        assert!(count(&out, "tibber") >= 4);
        assert_eq!(
            h.on_message("sim/meter/set", b"stop"),
            Err("expected pause or resume, got \"stop\"".into())
        );
    }

    #[test]
    fn outdoor_temperature_and_panel_setpoints_can_be_changed() {
        let (mut h, _) = house("");
        let out = h.on_message("sim/outdoor/set", b"-20").unwrap();
        assert_eq!(state(&out)["outdoor"]["value"], -20.0);
        assert_eq!(state(&out)["loads"]["heat_pump"]["value"], 1.5);
        assert_eq!(
            h.on_message("sim/outdoor/set", b"cold"),
            Err("not a number: \"cold\"".into())
        );
        let out = h.on_message("sim/zone/gang/setpoint/set", b"24").unwrap();
        assert_eq!(on(&out, "zwave/Gang/Gulv_gang/67/1/setpoint/1"), ["24"]);
        assert_eq!(state(&out)["floors"][2]["setpoint"]["value"], 24.0);
        assert_eq!(
            h.on_message("sim/zone/stue/setpoint/set", b"24"),
            Err("unknown zone \"stue\"".into())
        );
        assert_eq!(
            h.on_message("sim/zone/gang/setpoint/set", b"x"),
            Err("not a number: \"x\"".into())
        );
        assert_eq!(
            h.on_message("sim/bogus/set", b"1"),
            Err("unknown control topic \"sim/bogus/set\"".into())
        );
        assert_eq!(h.on_message("sim/ev", b"1"), Ok(vec![]));
        assert_eq!(h.on_message("elsewhere/set", b"1"), Ok(vec![]));
    }

    #[test]
    fn summary_line() {
        let (mut h, _) = house("");
        h.on_message("sim/ev/set", b"7.4").unwrap();
        h.on_message("sim/zone/bad/setpoint/set", b"40").unwrap();
        h.step(h.now() + secs(1));
        let line = h.summary();
        assert!(line.starts_with("Tue 16:00 "), "{line}");
        assert!(
            line.contains(" + ev 7.40 + extra 0.00; heating: bad"),
            "{line}"
        );
        h.on_message("sim/meter/set", b"pause").unwrap();
        for i in 0..6 {
            h.on_message(
                &format!(
                    "sim/zone/{}/setpoint/set",
                    h.floors()[i].spec().topics.zone.as_str()
                ),
                b"5",
            )
            .unwrap();
        }
        h.step(h.now() + secs(1));
        assert!(
            h.summary().ends_with("heating: none (meter paused)"),
            "{}",
            h.summary()
        );
    }

    #[test]
    fn stepping_backwards_changes_nothing() {
        let (mut h, _) = house("");
        h.step(h.now() + SignedDuration::from_mins(1));
        let now = h.now();
        let power = h.power();
        assert_eq!(h.step(now - secs(30)), vec![]);
        assert_eq!((h.now(), h.power()), (now, power));
    }

    #[test]
    fn the_meter_clock_is_local_time_with_the_dst_bit() {
        let (h, _) = house("");
        let t = h.meter_time();
        assert_eq!(t.datetime, jiff::civil::date(2026, 12, 1).at(16, 0, 0, 0));
        assert_eq!((t.deviation, t.dst), (None, Some(false)));
        let cfg = crate::parse(r#"start = "2026-07-01T12:00:00.75""#, &eb()).unwrap();
        let (summer, _) = House::new(&cfg);
        let t = summer.meter_time();
        assert_eq!(t.datetime, jiff::civil::date(2026, 7, 1).at(12, 0, 0, 0));
        assert_eq!(t.dst, Some(true));
    }

    #[test]
    fn helpers() {
        let every = secs(10);
        assert_eq!(
            aligned(ts("2026-12-01T15:00:00Z"), every),
            ts("2026-12-01T15:00:00Z")
        );
        assert_eq!(
            aligned(ts("2026-12-01T15:00:03.5Z"), every),
            ts("2026-12-01T15:00:10Z")
        );
        assert_eq!(round(1.23456, 2), 1.23);
    }

    /// Two evening hours, summarised every ten minutes: the EV plugs in at 17:00 and
    /// charges 5 kWh.
    #[test]
    fn evening_snapshot() {
        let (mut h, _) = house(
            "[[ev]]\nfrom = \"17:00\"\nkwh = 5\nkw = 7.4\n\
             [[appliance]]\nname = \"oven\"\nkw = 2.5\nminutes = 40\nper_day = 2\nfrom_hour = 16\nto_hour = 19",
        );
        let mut summary = String::new();
        let end = h.now() + SignedDuration::from_hours(2);
        while h.now() < end {
            let out = h.step(h.now() + SignedDuration::from_mins(10));
            let s = state(&out);
            let heating: Vec<&str> = s["floors"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|f| f["heating"] == true)
                .map(|f| f["zone"].as_str().unwrap())
                .collect();
            writeln!(
                summary,
                "{} {:>6.3} kW  ev {:>3}  appliances {:?}  heating {:?}",
                s["at"].as_str().unwrap(),
                s["power"]["value"].as_f64().unwrap(),
                s["loads"]["ev"]["value"],
                s["running"].as_array().unwrap(),
                heating,
            )
            .unwrap();
        }
        insta::assert_snapshot!(summary);
    }
}

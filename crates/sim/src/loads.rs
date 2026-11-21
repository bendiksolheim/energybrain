//! Everything in the house except the floors: background use, the heat pump, appliances,
//! the EV on its charger and extra load added by hand. Only the meter sees these (and Easee
//! sees the charger).
//!
//! The car draws `min(its demand, offered current × 230 V)`, nothing below 6 A, and follows a
//! new offer 20 s later, like a real car ramping. The charger offers the lower of the app's
//! charger limit and its dynamic current (energybrain's limit, which expires after its TTL,
//! or the user's temporary limit). Plugging in or out clears the dynamic current, as on an
//! Easee.

use energybrain_core::units::{Ampere, Celsius, Kw, Kwh, Volt};
use jiff::civil::Time;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

use crate::rng::Rng;

/// Outdoor temperature at or above which the heat pump is idle.
const HEAT_PUMP_IDLE_AT: f64 = 15.0;
/// Outdoor temperature at or below which the heat pump runs at full power.
const HEAT_PUMP_FULL_AT: f64 = -15.0;
/// Power per amp of offered current: one phase on a 230 V IT net, like the I-Pace.
const CHARGER_VOLTAGE: Volt = Volt::new(230.0);
/// Below this a car can't charge.
const MIN_CURRENT: Ampere = Ampere::new(6.0);
/// A car follows a new offer this much later.
const CAR_LAG: SignedDuration = SignedDuration::from_secs(20);

/// Background use by local hour of day (lights, fridge, cooking, ventilation, …).
#[derive(Debug, Clone, PartialEq)]
pub struct Background {
    /// Mean power for each local hour, 0–23.
    pub profile: [Kw; 24],
    /// Each minute a uniform deviation of up to this much is added.
    pub noise: Kw,
}

/// An appliance switched on at random moments, like a kettle or an oven.
#[derive(Debug, Clone, PartialEq)]
pub struct Appliance {
    /// For `sim/state`.
    pub name: String,
    /// Power while on.
    pub power: Kw,
    /// How long it runs once started.
    pub duration: SignedDuration,
    /// Mean starts per day, spread over the window.
    pub per_day: f64,
    /// First local hour it may start in.
    pub from_hour: i8,
    /// Local hour from which it no longer starts (exclusive).
    pub to_hour: i8,
}

/// A daily EV charging session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvSession {
    /// Local time the car is plugged in.
    pub from: Time,
    /// Energy to charge.
    pub energy: Kwh,
    /// What the car takes when offered enough.
    pub power: Kw,
}

/// What the charger shows (to Easee, and in `sim/state`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChargerState {
    /// A car is plugged in.
    pub plugged: bool,
    /// The plugged-in car is full.
    pub full: bool,
    /// What the car draws.
    pub power: Kw,
    /// Energy charged since plug-in.
    pub session: Kwh,
    /// The dynamic current, if one is in force.
    pub dynamic: Option<Ampere>,
    /// The app's "Charger limit".
    pub limit: Ampere,
    /// What the pilot signal offers the car.
    pub offered: Ampere,
}

/// What each kind of load draws right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Breakdown {
    /// Background use.
    pub background: Kw,
    /// Heat pump.
    pub heat_pump: Kw,
    /// Appliances that are on.
    pub appliances: Kw,
    /// EV charger.
    pub ev: Kw,
    /// Extra load from `sim/load/set`.
    pub extra: Kw,
}

impl Breakdown {
    /// The sum.
    pub fn total(&self) -> Kw {
        self.background + self.heat_pump + self.appliances + self.ev + self.extra
    }
}

/// A plugged-in car.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Car {
    demand: Kw,
    /// `None` for a manual session that runs until switched off.
    remaining: Option<Kwh>,
    session: Kwh,
    drawing: Kw,
    /// When `drawing` catches up with what the car would take now.
    follows_at: Option<Timestamp>,
}

impl Car {
    /// A car plugged in at `at`; it starts drawing `CAR_LAG` later.
    fn new(demand: Kw, remaining: Option<Kwh>, at: Timestamp) -> Self {
        Self {
            demand,
            remaining,
            session: Kwh::ZERO,
            drawing: Kw::ZERO,
            follows_at: Some(at + CAR_LAG),
        }
    }

    fn full(&self) -> bool {
        self.remaining.is_some_and(|r| r <= Kwh::ZERO)
    }

    /// What the car would take if offered `offered`.
    fn wanted(&self, offered: Ampere) -> Kw {
        if self.full() || offered < MIN_CURRENT {
            Kw::ZERO
        } else {
            self.demand.min(offered * CHARGER_VOLTAGE)
        }
    }
}

/// The loads and their state.
#[derive(Debug, Clone)]
pub struct Loads {
    tz: TimeZone,
    background: Background,
    heat_pump_max: Kw,
    appliances: Vec<(Appliance, Option<Timestamp>)>,
    sessions: Vec<EvSession>,
    car: Option<Car>,
    charger_limit: Ampere,
    /// The dynamic current and when it expires (`None`: until replaced or unplugged).
    dynamic: Option<(Ampere, Option<Timestamp>)>,
    extra: Kw,
    outdoor: Celsius,
    noise: Kw,
}

impl Loads {
    /// Loads at rest: no appliance on, no car plugged in. `charger_limit` is the app's
    /// "Charger limit".
    pub fn new(
        tz: TimeZone,
        background: Background,
        heat_pump_max: Kw,
        appliances: Vec<Appliance>,
        sessions: Vec<EvSession>,
        charger_limit: Ampere,
        outdoor: Celsius,
    ) -> Self {
        Self {
            tz,
            background,
            heat_pump_max,
            appliances: appliances.into_iter().map(|a| (a, None)).collect(),
            sessions,
            car: None,
            charger_limit,
            dynamic: None,
            extra: Kw::ZERO,
            outdoor,
            noise: Kw::ZERO,
        }
    }

    /// Once a minute at `at`: new background noise, random appliance starts, and EV sessions
    /// whose plug-in time this is.
    pub fn minute(&mut self, at: Timestamp, rng: &mut Rng) {
        self.noise = self.background.noise * rng.symmetric();
        let local = self.tz.to_datetime(at);
        for (appliance, until) in &mut self.appliances {
            let window = appliance.to_hour - appliance.from_hour;
            let open = (appliance.from_hour..appliance.to_hour).contains(&local.hour());
            let p = appliance.per_day / (f64::from(window) * 60.0);
            if until.is_none() && open && rng.chance(p) {
                *until = Some(at + appliance.duration);
            }
        }
        let plug_in = self
            .sessions
            .iter()
            .find(|s| local.hour() == s.from.hour() && local.minute() == s.from.minute())
            .map(|s| Car::new(s.power, Some(s.energy), at));
        if let Some(car) = plug_in {
            self.plug(Some(car));
        }
    }

    /// Plugs a car in (or out, with `None`); either clears the dynamic current.
    fn plug(&mut self, car: Option<Car>) {
        self.car = car;
        self.dynamic = None;
    }

    /// What the charger offers the car.
    fn offered(&self) -> Ampere {
        self.dynamic
            .map_or(self.charger_limit, |(a, _)| a.min(self.charger_limit))
    }

    /// Lets time pass until `to` after `dt`: appliances finish and the car charges.
    pub fn advance(&mut self, dt: SignedDuration, to: Timestamp) {
        for (_, until) in &mut self.appliances {
            if until.is_some_and(|u| u <= to) {
                *until = None;
            }
        }
        self.dynamic = self
            .dynamic
            .filter(|(_, until)| until.is_none_or(|u| u > to));
        let offered = self.offered();
        if let Some(car) = &mut self.car {
            let charged = car.drawing * dt;
            car.session += charged;
            if let Some(left) = &mut car.remaining {
                *left -= charged;
            }
            if car.full() {
                car.drawing = Kw::ZERO;
            }
            let wanted = car.wanted(offered);
            car.follows_at = match car.follows_at {
                _ if (wanted - car.drawing).value().abs() < 1e-9 => None,
                Some(at) if to >= at => {
                    car.drawing = wanted;
                    None
                }
                Some(at) => Some(at),
                None => Some(to + CAR_LAG),
            };
        }
    }

    /// What the charger shows at the moment.
    pub fn charger(&self) -> ChargerState {
        ChargerState {
            plugged: self.car.is_some(),
            full: self.car.is_some_and(|c| c.full()),
            power: self.car.map_or(Kw::ZERO, |c| c.drawing),
            session: self.car.map_or(Kwh::ZERO, |c| c.session),
            dynamic: self.dynamic.map(|(a, _)| a),
            limit: self.charger_limit,
            offered: if self.car.is_some() {
                self.offered()
            } else {
                Ampere::ZERO
            },
        }
    }

    /// Sets the charger's dynamic current until `until` (`None`: until replaced or
    /// unplugged), as Easee's `set_dynamic_charger_current` or the app's temporary limit do.
    pub fn set_dynamic(&mut self, current: Option<Ampere>, until: Option<Timestamp>) {
        self.dynamic = current.map(|a| (a, until));
    }

    /// What each load draws at `at`.
    pub fn breakdown(&self, at: Timestamp) -> Breakdown {
        let hour = usize::try_from(self.tz.to_datetime(at).hour()).unwrap_or_default();
        let base = self
            .background
            .profile
            .get(hour)
            .copied()
            .unwrap_or_default();
        let share = ((HEAT_PUMP_IDLE_AT - self.outdoor.value())
            / (HEAT_PUMP_IDLE_AT - HEAT_PUMP_FULL_AT))
            .clamp(0.0, 1.0);
        Breakdown {
            background: (base + self.noise).max(Kw::ZERO),
            heat_pump: self.heat_pump_max * share,
            appliances: self
                .appliances
                .iter()
                .filter(|(_, until)| until.is_some())
                .map(|(a, _)| a.power)
                .sum(),
            ev: self.car.map_or(Kw::ZERO, |c| c.drawing),
            extra: self.extra,
        }
    }

    /// Names of the appliances that are on.
    pub fn running(&self) -> Vec<&str> {
        self.appliances
            .iter()
            .filter(|(_, until)| until.is_some())
            .map(|(a, _)| a.name.as_str())
            .collect()
    }

    /// Plugs in a car at `at` that takes `power` until unplugged, or unplugs it (`None`).
    pub fn set_ev(&mut self, power: Option<Kw>, at: Timestamp) {
        self.plug(power.map(|p| Car::new(p, None, at)));
    }

    /// Extra constant load, replacing the previous one.
    pub fn set_extra(&mut self, power: Kw) {
        self.extra = power;
    }

    /// The outdoor temperature.
    pub fn outdoor(&self) -> Celsius {
        self.outdoor
    }

    /// Changes the outdoor temperature.
    pub fn set_outdoor(&mut self, t: Celsius) {
        self.outdoor = t;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn oslo() -> TimeZone {
        TimeZone::get("Europe/Oslo").unwrap()
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    pub(crate) fn kettle() -> Appliance {
        Appliance {
            name: "kettle".into(),
            power: Kw::new(2.0),
            duration: SignedDuration::from_mins(3),
            per_day: 4.0,
            from_hour: 6,
            to_hour: 23,
        }
    }

    fn loads(appliances: Vec<Appliance>, sessions: Vec<EvSession>) -> Loads {
        let mut profile = [Kw::new(0.3); 24];
        profile[18] = Kw::new(1.6);
        Loads::new(
            oslo(),
            Background {
                profile,
                noise: Kw::new(0.1),
            },
            Kw::new(1.5),
            appliances,
            sessions,
            Ampere::new(16.0),
            Celsius::new(-5.0),
        )
    }

    #[test]
    fn background_follows_the_local_hour_and_the_heat_pump_the_weather() {
        let mut l = loads(vec![], vec![]);
        // 17:30Z is 18:30 in Oslo in winter.
        let b = l.breakdown(ts("2026-12-01T17:30:00Z"));
        assert_eq!(b.background, Kw::new(1.6));
        assert_eq!(b.heat_pump, Kw::new(1.0));
        assert_eq!(b.total(), Kw::new(2.6));
        assert_eq!(l.outdoor(), Celsius::new(-5.0));
        l.set_outdoor(Celsius::new(20.0));
        assert_eq!(l.breakdown(ts("2026-12-01T17:30:00Z")).heat_pump, Kw::ZERO);
        l.set_outdoor(Celsius::new(-30.0));
        assert_eq!(
            l.breakdown(ts("2026-12-01T17:30:00Z")).heat_pump,
            Kw::new(1.5)
        );
    }

    #[test]
    fn noise_changes_each_minute_and_never_goes_negative() {
        let mut l = loads(vec![], vec![]);
        let mut rng = Rng::new(3);
        let at = ts("2026-12-01T02:00:00Z");
        let mut seen = Vec::new();
        for _ in 0..50 {
            l.minute(at, &mut rng);
            let b = l.breakdown(at).background;
            assert!(b >= Kw::new(0.2) && b <= Kw::new(0.4), "{b}");
            seen.push(b);
        }
        seen.dedup();
        assert!(seen.len() > 40);
        let mut l = Loads {
            noise: Kw::new(-1.0),
            ..loads(vec![], vec![])
        };
        assert_eq!(l.breakdown(at).background, Kw::ZERO);
        l.set_extra(Kw::new(3.0));
        assert_eq!(l.breakdown(at).extra, Kw::new(3.0));
    }

    #[test]
    fn appliances_start_at_random_inside_their_window_and_finish() {
        let mut l = loads(vec![kettle()], vec![]);
        let mut rng = Rng::new(1);
        // At night (03:00 local) it never starts.
        for _ in 0..10_000 {
            l.minute(ts("2026-12-01T02:00:00Z"), &mut rng);
        }
        assert_eq!(l.running(), Vec::<&str>::new());
        // In the evening it starts sooner or later: ~4 per 17 h → p ≈ 0.4 % per minute.
        let mut t = ts("2026-12-01T17:00:00Z");
        while l.running().is_empty() {
            l.minute(t, &mut rng);
            t += SignedDuration::from_mins(1);
        }
        assert_eq!(l.running(), ["kettle"]);
        assert_eq!(l.breakdown(t).appliances, Kw::new(2.0));
        // Still on after two minutes, off after three.
        l.advance(
            SignedDuration::from_mins(2),
            t + SignedDuration::from_mins(1),
        );
        assert_eq!(l.running(), ["kettle"]);
        l.advance(
            SignedDuration::from_mins(1),
            t + SignedDuration::from_mins(2),
        );
        assert_eq!(l.running(), Vec::<&str>::new());
        assert_eq!(l.breakdown(t).appliances, Kw::ZERO);
    }

    /// Advances `l` from `from` in 1 s steps for `secs` seconds.
    fn run(l: &mut Loads, from: Timestamp, secs: i64) -> Timestamp {
        let mut t = from;
        for _ in 0..secs {
            t += SignedDuration::from_secs(1);
            l.advance(SignedDuration::from_secs(1), t);
        }
        t
    }

    #[test]
    fn ev_sessions_plug_in_ramp_up_and_stop_when_charged() {
        let session = EvSession {
            from: Time::constant(17, 30, 0, 0),
            energy: Kwh::new(0.5),
            power: Kw::new(3.0),
        };
        let mut l = loads(vec![], vec![session]);
        let mut rng = Rng::new(1);
        l.minute(ts("2026-12-01T16:29:00Z"), &mut rng);
        assert!(!l.charger().plugged);
        assert_eq!(l.charger().offered, Ampere::ZERO);
        let t = ts("2026-12-01T16:30:00Z");
        l.minute(t, &mut rng);
        assert!(l.charger().plugged);
        // The car starts 20 s after plug-in.
        let t = run(&mut l, t, 19);
        assert_eq!(l.breakdown(t).ev, Kw::ZERO);
        let t = run(&mut l, t, 1);
        assert_eq!(l.breakdown(t).ev, Kw::new(3.0));
        // 0.5 kWh at 3 kW takes 10 minutes; then it is full and draws nothing.
        let t = run(&mut l, t, 600);
        let c = l.charger();
        assert!(c.full && c.plugged);
        assert_eq!(l.breakdown(t).ev, Kw::ZERO);
        assert!((c.session.value() - 0.5).abs() < 1e-9, "{}", c.session);
    }

    #[test]
    fn the_car_follows_the_offer_and_limits_expire() {
        let mut l = loads(vec![], vec![]);
        let t = ts("2026-12-01T10:00:00Z");
        l.set_ev(Some(Kw::new(7.4)), t);
        let t = run(&mut l, t, 25);
        // The app's 16 A charger limit caps a car that would take 7.4 kW.
        assert_eq!(l.breakdown(t).ev, Ampere::new(16.0) * CHARGER_VOLTAGE);
        // energybrain limits it to 9 A for a minute: followed 20 s later, then it expires.
        l.set_dynamic(
            Some(Ampere::new(9.0)),
            Some(t + SignedDuration::from_mins(1)),
        );
        assert_eq!(l.charger().dynamic, Some(Ampere::new(9.0)));
        assert_eq!(l.charger().offered, Ampere::new(9.0));
        let t = run(&mut l, t, 21);
        assert_eq!(l.breakdown(t).ev, Ampere::new(9.0) * CHARGER_VOLTAGE);
        let t = run(&mut l, t, 40);
        assert_eq!(l.charger().dynamic, None);
        let t = run(&mut l, t, 21);
        assert_eq!(l.breakdown(t).ev, Ampere::new(16.0) * CHARGER_VOLTAGE);
        // Below 6 A nothing flows; a limit without expiry stays until unplugged.
        l.set_dynamic(Some(Ampere::new(5.0)), None);
        let t = run(&mut l, t, 21);
        assert_eq!(l.breakdown(t).ev, Kw::ZERO);
        assert!(!l.charger().full);
        l.set_ev(None, t);
        assert_eq!(l.charger().dynamic, None, "unplugging clears it");
        assert_eq!(l.breakdown(t).ev, Kw::ZERO);
    }
}

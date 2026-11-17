//! Everything in the house except the floors: background use, the heat pump, appliances,
//! the EV charger and extra load added by hand. Only the meter sees these.

use energybrain_core::units::{Celsius, Kw, Kwh};
use jiff::civil::Time;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

use crate::rng::Rng;

/// Outdoor temperature at or above which the heat pump is idle.
const HEAT_PUMP_IDLE_AT: f64 = 15.0;
/// Outdoor temperature at or below which the heat pump runs at full power.
const HEAT_PUMP_FULL_AT: f64 = -15.0;

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
    /// Charging power.
    pub power: Kw,
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

#[derive(Debug, Clone, Copy, PartialEq)]
struct Charging {
    power: Kw,
    /// `None` for a manual session that runs until switched off.
    remaining: Option<Kwh>,
}

/// The loads and their state.
#[derive(Debug, Clone)]
pub struct Loads {
    tz: TimeZone,
    background: Background,
    heat_pump_max: Kw,
    appliances: Vec<(Appliance, Option<Timestamp>)>,
    sessions: Vec<EvSession>,
    ev: Option<Charging>,
    extra: Kw,
    outdoor: Celsius,
    noise: Kw,
}

impl Loads {
    /// Loads at rest: no appliance on, no car charging.
    pub fn new(
        tz: TimeZone,
        background: Background,
        heat_pump_max: Kw,
        appliances: Vec<Appliance>,
        sessions: Vec<EvSession>,
        outdoor: Celsius,
    ) -> Self {
        Self {
            tz,
            background,
            heat_pump_max,
            appliances: appliances.into_iter().map(|a| (a, None)).collect(),
            sessions,
            ev: None,
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
        for s in &self.sessions {
            if local.hour() == s.from.hour() && local.minute() == s.from.minute() {
                self.ev = Some(Charging {
                    power: s.power,
                    remaining: Some(s.energy),
                });
            }
        }
    }

    /// Lets time pass until `to` after `dt`: appliances finish and the car charges.
    pub fn advance(&mut self, dt: SignedDuration, to: Timestamp) {
        for (_, until) in &mut self.appliances {
            if until.is_some_and(|u| u <= to) {
                *until = None;
            }
        }
        if let Some(Charging {
            power,
            remaining: Some(left),
        }) = &mut self.ev
        {
            *left -= *power * dt;
            if *left <= Kwh::ZERO {
                self.ev = None;
            }
        }
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
            ev: self.ev.map_or(Kw::ZERO, |c| c.power),
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

    /// Starts charging at `power` until switched off, or stops charging (`None`).
    pub fn set_ev(&mut self, power: Option<Kw>) {
        self.ev = power.map(|power| Charging {
            power,
            remaining: None,
        });
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

    #[test]
    fn ev_sessions_start_at_plug_in_and_stop_when_charged() {
        let session = EvSession {
            from: Time::constant(17, 30, 0, 0),
            energy: Kwh::new(1.0),
            power: Kw::new(6.0),
        };
        let mut l = loads(vec![], vec![session]);
        let mut rng = Rng::new(1);
        l.minute(ts("2026-12-01T16:29:00Z"), &mut rng);
        assert_eq!(l.breakdown(ts("2026-12-01T16:29:00Z")).ev, Kw::ZERO);
        l.minute(ts("2026-12-01T16:30:00Z"), &mut rng);
        assert_eq!(l.breakdown(ts("2026-12-01T16:30:00Z")).ev, Kw::new(6.0));
        // 1 kWh at 6 kW takes 10 minutes.
        l.advance(SignedDuration::from_mins(9), ts("2026-12-01T16:39:00Z"));
        assert_eq!(l.breakdown(ts("2026-12-01T16:39:00Z")).ev, Kw::new(6.0));
        l.advance(SignedDuration::from_mins(1), ts("2026-12-01T16:40:00Z"));
        assert_eq!(l.breakdown(ts("2026-12-01T16:40:00Z")).ev, Kw::ZERO);
    }

    #[test]
    fn manual_ev_charging_runs_until_switched_off() {
        let mut l = loads(vec![], vec![]);
        l.set_ev(Some(Kw::new(7.4)));
        l.advance(SignedDuration::from_hours(10), ts("2026-12-01T10:00:00Z"));
        assert_eq!(l.breakdown(ts("2026-12-01T10:00:00Z")).ev, Kw::new(7.4));
        l.set_ev(None);
        assert_eq!(l.breakdown(ts("2026-12-01T10:00:00Z")).ev, Kw::ZERO);
    }
}

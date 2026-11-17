//! One floor-heating zone: the slab's temperature and its Heatit Z-TRM3 thermostat.
//!
//! The slab is a single thermal mass: `capacity · dT/dt = heater − loss · (T − environment)`.
//! The thermostat switches the heater on below `setpoint − 0.5 °C` and off above
//! `setpoint + 0.5 °C`, so a shed (setpoint lowered by 4 °C) keeps the heater off until the
//! floor has really cooled, as it does in the house.

use energybrain_config::ZoneTopics;
use energybrain_core::units::{Celsius, CelsiusDelta, Kw, Kwh};
use jiff::SignedDuration;

/// Half the thermostat's switching band.
pub const HYSTERESIS: CelsiusDelta = CelsiusDelta::new(0.5);
/// Lowest setpoint a Z-TRM3 accepts.
pub const SETPOINT_MIN: Celsius = Celsius::new(5.0);
/// Highest setpoint a Z-TRM3 accepts.
pub const SETPOINT_MAX: Celsius = Celsius::new(40.0);
/// The thermostat's setpoint resolution.
const SETPOINT_STEP: f64 = 0.5;
/// What a Z-TRM3 reports while idle (about 0.1 W).
pub const IDLE: Kw = Kw::new(0.0001);
/// Longest integration step; floors are slow, so this is far below their time constant.
const MAX_SLICE: SignedDuration = SignedDuration::from_secs(10);

/// Heat loss of a floor, in kW per kelvin of difference to its environment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeatLoss(f64);

impl HeatLoss {
    /// Wraps a value in kW/K.
    pub const fn new(kw_per_k: f64) -> Self {
        Self(kw_per_k)
    }

    /// The value in kW/K.
    pub const fn value(self) -> f64 {
        self.0
    }
}

/// Heat capacity of a floor, in kWh per kelvin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeatCapacity(f64);

impl HeatCapacity {
    /// Wraps a value in kWh/K.
    pub const fn new(kwh_per_k: f64) -> Self {
        Self(kwh_per_k)
    }

    /// The value in kWh/K.
    pub const fn value(self) -> f64 {
        self.0
    }
}

/// What a floor is made of.
#[derive(Debug, Clone, PartialEq)]
pub struct FloorSpec {
    /// The thermostat's topics, from the energybrain configuration.
    pub topics: ZoneTopics,
    /// Heater power while on.
    pub heater: Kw,
    /// The thermostat's setpoint at the start.
    pub setpoint: Celsius,
    /// Thermal mass.
    pub capacity: HeatCapacity,
    /// Heat loss to the environment.
    pub loss: HeatLoss,
    /// The temperature the floor loses heat to, if not the house's (e.g. a cold basement).
    pub ambient: Option<Celsius>,
}

/// A floor's state.
#[derive(Debug, Clone)]
pub struct Floor {
    spec: FloorSpec,
    temperature: Celsius,
    setpoint: Celsius,
    heating: bool,
    energy: Kwh,
}

impl Floor {
    /// A floor at `setpoint + offset`, heating if that is below the setpoint.
    pub fn new(spec: FloorSpec, offset: CelsiusDelta) -> Self {
        let setpoint = spec.setpoint;
        let temperature = setpoint + offset;
        Self {
            spec,
            temperature,
            setpoint,
            heating: temperature < setpoint,
            energy: Kwh::ZERO,
        }
    }

    /// What the floor is made of.
    pub fn spec(&self) -> &FloorSpec {
        &self.spec
    }

    /// The slab temperature.
    pub fn temperature(&self) -> Celsius {
        self.temperature
    }

    /// The thermostat's setpoint.
    pub fn setpoint(&self) -> Celsius {
        self.setpoint
    }

    /// Whether the heater is on.
    pub fn heating(&self) -> bool {
        self.heating
    }

    /// The thermostat's cumulative energy counter.
    pub fn energy(&self) -> Kwh {
        self.energy
    }

    /// What the thermostat draws right now.
    pub fn power(&self) -> Kw {
        if self.heating { self.spec.heater } else { IDLE }
    }

    /// Sets the thermostat's setpoint as a Z-TRM3 would: clamped to 5–40 °C and rounded to
    /// its 0.5 °C step. Returns the value it now reports.
    pub fn set_setpoint(&mut self, wanted: Celsius) -> Celsius {
        let clamped = wanted.clamp(SETPOINT_MIN, SETPOINT_MAX).value();
        self.setpoint = Celsius::new((clamped / SETPOINT_STEP).round() * SETPOINT_STEP);
        self.setpoint
    }

    /// Lets `dt` pass with the house at `indoor`. Returns whether the heater switched.
    pub fn advance(&mut self, dt: SignedDuration, indoor: Celsius) -> bool {
        let environment = self.spec.ambient.unwrap_or(indoor);
        let before = self.heating;
        let mut left = dt;
        while left.is_positive() {
            let slice = left.min(MAX_SLICE);
            let power = self.power();
            self.energy += power * slice;
            let loss = self.spec.loss.value() * (self.temperature - environment).value();
            let rise = (power.value() - loss) / self.spec.capacity.value() * hours(slice);
            self.temperature = self.temperature + CelsiusDelta::new(rise);
            self.heating = if self.heating {
                self.temperature < self.setpoint + HYSTERESIS
            } else {
                self.temperature < self.setpoint - HYSTERESIS
            };
            left -= slice;
        }
        self.heating != before
    }
}

fn hours(d: SignedDuration) -> f64 {
    d.as_secs_f64() / 3600.0
}

#[cfg(test)]
pub(crate) mod tests {
    use energybrain_core::ids::ZoneId;

    use super::*;

    pub(crate) fn spec(id: &str) -> FloorSpec {
        let device = format!("zwave/{id}");
        FloorSpec {
            topics: ZoneTopics {
                zone: ZoneId::new(id).unwrap(),
                setpoint: format!("{device}/67/1/setpoint/1"),
                power: format!("{device}/50/1/value/66049"),
                energy: format!("{device}/50/1/value/65537"),
            },
            heater: Kw::new(1.5),
            setpoint: Celsius::new(26.0),
            capacity: HeatCapacity::new(0.5),
            loss: HeatLoss::new(0.08),
            ambient: None,
        }
    }

    #[test]
    fn units_wrap_their_value() {
        assert_eq!(HeatLoss::new(0.1).value(), 0.1);
        assert_eq!(HeatCapacity::new(0.5).value(), 0.5);
    }

    #[test]
    fn starts_heating_only_below_its_setpoint() {
        let cold = Floor::new(spec("bad"), CelsiusDelta::new(-0.2));
        assert!(cold.heating());
        assert_eq!(cold.power(), Kw::new(1.5));
        assert_eq!(cold.temperature(), Celsius::new(25.8));
        assert_eq!(cold.spec().heater, Kw::new(1.5));
        let warm = Floor::new(spec("bad"), CelsiusDelta::new(0.2));
        assert!(!warm.heating());
        assert_eq!(warm.power(), IDLE);
        assert_eq!(warm.setpoint(), Celsius::new(26.0));
        assert_eq!(warm.energy(), Kwh::ZERO);
    }

    #[test]
    fn setpoints_are_clamped_and_rounded_like_a_z_trm3() {
        let mut f = Floor::new(spec("bad"), CelsiusDelta::ZERO);
        assert_eq!(f.set_setpoint(Celsius::new(22.2)), Celsius::new(22.0));
        assert_eq!(f.set_setpoint(Celsius::new(22.3)), Celsius::new(22.5));
        assert_eq!(f.set_setpoint(Celsius::new(1.0)), SETPOINT_MIN);
        assert_eq!(f.set_setpoint(Celsius::new(55.0)), SETPOINT_MAX);
        assert_eq!(f.setpoint(), SETPOINT_MAX);
    }

    /// Runs until the heater switches, returning how long that took.
    fn until_switch(f: &mut Floor, indoor: Celsius) -> SignedDuration {
        let mut t = SignedDuration::ZERO;
        while !f.advance(SignedDuration::from_secs(30), indoor) {
            t += SignedDuration::from_secs(30);
            assert!(t < SignedDuration::from_hours(48), "never switched");
        }
        t
    }

    #[test]
    fn cycles_within_the_hysteresis_band() {
        let indoor = Celsius::new(15.0);
        let mut f = Floor::new(spec("bad"), CelsiusDelta::new(-0.4));
        // Heats up to 26.5, then cools down to 25.5, and so on: tens of minutes each way.
        let on = until_switch(&mut f, indoor);
        assert!(!f.heating());
        assert!(f.temperature() >= Celsius::new(26.5));
        let off = until_switch(&mut f, indoor);
        assert!(f.heating());
        assert!(f.temperature() < Celsius::new(25.5));
        for d in [on, off] {
            assert!(d > SignedDuration::from_mins(15) && d < SignedDuration::from_hours(2));
        }
        // It used the heater's power while on.
        let expected = Kw::new(1.5) * on;
        assert!(
            (f.energy() - expected).value().abs() < 0.1,
            "{}",
            f.energy()
        );
    }

    #[test]
    fn a_shed_keeps_the_heater_off_for_hours() {
        let mut f = Floor::new(spec("bad"), CelsiusDelta::new(-0.4));
        f.set_setpoint(Celsius::new(22.0));
        assert!(f.advance(SignedDuration::from_secs(1), Celsius::new(15.0)));
        assert!(!f.heating());
        let off = until_switch(&mut f, Celsius::new(15.0));
        assert!(off > SignedDuration::from_hours(2), "{off}");
    }

    #[test]
    fn its_own_ambient_overrides_the_house() {
        // A basement floor at 15 °C losing heat to 15 °C needs no heat at all.
        let mut s = spec("kjeller");
        s.setpoint = Celsius::new(15.0);
        s.ambient = Some(Celsius::new(15.0));
        let mut f = Floor::new(s, CelsiusDelta::ZERO);
        assert!(!f.advance(SignedDuration::from_hours(12), Celsius::new(-20.0)));
        // Only the thermostat's 0.1 W idle draw warms it, by a few thousandths of a degree.
        assert!((f.temperature() - Celsius::new(15.0)).value().abs() < 0.01);
        assert!(!f.heating());
    }

    #[test]
    fn no_time_no_change() {
        let mut f = Floor::new(spec("bad"), CelsiusDelta::new(-1.0));
        assert!(!f.advance(SignedDuration::ZERO, Celsius::new(15.0)));
        assert!(!f.advance(SignedDuration::from_secs(-5), Celsius::new(15.0)));
        assert_eq!(f.temperature(), Celsius::new(25.0));
    }
}

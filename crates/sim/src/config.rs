//! `sim.toml` → validated [`SimConfig`], checked against the energybrain configuration so
//! the simulated thermostats always publish where energybrain listens.

use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

use energybrain_config::Config;
use energybrain_core::mode::Mode;
use energybrain_core::units::{Ampere, Celsius, Kw, Kwh};
use jiff::civil::{DateTime, Time};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

use crate::floor::{FloorSpec, HeatCapacity, HeatLoss, SETPOINT_MAX, SETPOINT_MIN};
use crate::loads::{Appliance, Background, EvSession};

/// Highest speed-up: one simulated hour per real second.
pub const MAX_FACTOR: u16 = 3600;

/// Why `sim.toml` was rejected.
#[derive(Debug, thiserror::Error)]
pub enum SimConfigError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The I/O error.
        source: std::io::Error,
    },
    /// Not valid TOML, or a key has the wrong type or is unknown.
    #[error("{0}")]
    Parse(#[from] toml::de::Error),
    /// A value is out of range or inconsistent.
    #[error("{key}: {reason}")]
    Invalid {
        /// Dotted path of the offending key.
        key: String,
        /// What is wrong.
        reason: String,
    },
}

fn invalid(key: impl Into<String>, reason: impl Into<String>) -> SimConfigError {
    SimConfigError::Invalid {
        key: key.into(),
        reason: reason.into(),
    }
}

/// The validated simulation settings, plus what it needs from the energybrain config.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// Simulated seconds per real second.
    pub factor: NonZeroU16,
    /// Simulated instant the run starts at.
    pub start: Timestamp,
    /// Seed for every random choice.
    pub seed: u64,
    /// The mode to start energybrain in, if set (written to its fresh database).
    pub initial_mode: Option<Mode>,
    /// Outdoor temperature at the start.
    pub outdoor: Celsius,
    /// The house's indoor temperature. Floors lose heat to a mix of it and the outdoor
    /// temperature (see `house::OUTDOOR_SHARE`).
    pub indoor: Celsius,
    /// The meter's cumulative import register at the start.
    pub register: Kwh,
    /// Background use.
    pub background: Background,
    /// Heat-pump power at full load.
    pub heat_pump: Kw,
    /// Appliances started at random.
    pub appliances: Vec<Appliance>,
    /// Daily EV charging sessions.
    pub ev: Vec<EvSession>,
    /// The charger's limit set in the Easee app ("Charger limit").
    pub charger_limit: Ampere,
    /// One per energybrain zone, in its order.
    pub floors: Vec<FloorSpec>,
    /// The meter topic.
    pub meter_topic: String,
    /// The house's time zone.
    pub tz: TimeZone,
}

/// Reads and validates `path` against the energybrain config `eb`.
pub fn load(path: &Path, eb: &Config) -> Result<SimConfig, SimConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| SimConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    parse(&text, eb)
}

/// Parses and validates `sim.toml` text against the energybrain config `eb`.
pub fn parse(text: &str, eb: &Config) -> Result<SimConfig, SimConfigError> {
    let raw: Raw = toml::from_str(text)?;
    raw.validate(eb)
}

/// A typical house without floor heating, heat pump or EV: low at night, peaks at breakfast
/// and dinner.
const DEFAULT_PROFILE: [f64; 24] = [
    0.35, 0.3, 0.3, 0.3, 0.3, 0.35, 0.8, 1.2, 1.0, 0.7, 0.6, 0.6, 0.7, 0.6, 0.6, 0.7, 1.0, 1.4,
    1.6, 1.4, 1.1, 0.9, 0.7, 0.5,
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(default = "default_factor")]
    factor: u16,
    start: String,
    #[serde(default = "default_seed")]
    seed: u64,
    initial_mode: Option<String>,
    #[serde(default = "default_outdoor")]
    outdoor_c: f64,
    #[serde(default = "default_indoor")]
    indoor_c: f64,
    #[serde(default = "default_register")]
    register_kwh: f64,
    #[serde(default)]
    background: RawBackground,
    #[serde(default)]
    heat_pump: RawHeatPump,
    #[serde(default, rename = "appliance")]
    appliances: Vec<RawAppliance>,
    #[serde(default)]
    ev: Vec<RawEv>,
    #[serde(default = "default_charger_limit")]
    charger_limit_a: f64,
    #[serde(default, rename = "floor")]
    floors: Vec<RawFloor>,
}

fn default_charger_limit() -> f64 {
    32.0
}
fn default_factor() -> u16 {
    6
}
fn default_seed() -> u64 {
    1
}
fn default_outdoor() -> f64 {
    -5.0
}
fn default_indoor() -> f64 {
    20.0
}
fn default_register() -> f64 {
    10_000.0
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackground {
    #[serde(default = "default_profile")]
    profile_kw: Vec<f64>,
    #[serde(default = "default_noise")]
    noise_kw: f64,
}

impl Default for RawBackground {
    fn default() -> Self {
        Self {
            profile_kw: default_profile(),
            noise_kw: default_noise(),
        }
    }
}

fn default_profile() -> Vec<f64> {
    DEFAULT_PROFILE.to_vec()
}
fn default_noise() -> f64 {
    0.15
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHeatPump {
    #[serde(default = "default_heat_pump")]
    max_kw: f64,
}

impl Default for RawHeatPump {
    fn default() -> Self {
        Self {
            max_kw: default_heat_pump(),
        }
    }
}

fn default_heat_pump() -> f64 {
    1.5
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAppliance {
    name: String,
    kw: f64,
    minutes: u32,
    per_day: f64,
    #[serde(default = "default_from_hour")]
    from_hour: i8,
    #[serde(default = "default_to_hour")]
    to_hour: i8,
}

fn default_from_hour() -> i8 {
    6
}
fn default_to_hour() -> i8 {
    23
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEv {
    from: String,
    kwh: f64,
    kw: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFloor {
    zone: String,
    heater_kw: Option<f64>,
    #[serde(default = "default_setpoint")]
    setpoint: f64,
    #[serde(default = "default_capacity")]
    capacity_kwh_per_k: f64,
    #[serde(default = "default_loss")]
    loss_kw_per_k: f64,
    ambient_c: Option<f64>,
}

fn default_setpoint() -> f64 {
    22.0
}
fn default_capacity() -> f64 {
    0.5
}
fn default_loss() -> f64 {
    0.08
}

fn finite(key: &str, v: f64) -> Result<f64, SimConfigError> {
    if v.is_finite() {
        Ok(v)
    } else {
        Err(invalid(key, format!("{v} is not a number")))
    }
}

fn non_negative(key: &str, v: f64) -> Result<f64, SimConfigError> {
    if finite(key, v)? >= 0.0 {
        Ok(v)
    } else {
        Err(invalid(key, format!("{v} is negative")))
    }
}

fn positive(key: &str, v: f64) -> Result<f64, SimConfigError> {
    if finite(key, v)? > 0.0 {
        Ok(v)
    } else {
        Err(invalid(key, format!("{v} must be above 0")))
    }
}

impl Raw {
    fn validate(self, eb: &Config) -> Result<SimConfig, SimConfigError> {
        let tz = eb.engine.calendar.time_zone().clone();
        let factor = NonZeroU16::new(self.factor)
            .filter(|f| f.get() <= MAX_FACTOR)
            .ok_or_else(|| {
                invalid(
                    "factor",
                    format!("{} is not between 1 and {MAX_FACTOR}", self.factor),
                )
            })?;
        let start = self
            .start
            .parse::<DateTime>()
            .and_then(|dt| tz.to_ambiguous_timestamp(dt).compatible())
            .map_err(|e| invalid("start", format!("{:?}: {e}", self.start)))?;
        let initial_mode = self
            .initial_mode
            .map(|m| Mode::parse(&m).ok_or_else(|| invalid("initial_mode", format!("{m:?}"))))
            .transpose()?;
        let profile: [Kw; 24] = self
            .background
            .profile_kw
            .iter()
            .enumerate()
            .map(|(h, &v)| non_negative(&format!("background.profile_kw[{h}]"), v).map(Kw::new))
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
            .map_err(|v: Vec<Kw>| {
                invalid(
                    "background.profile_kw",
                    format!("has {} values, needs 24", v.len()),
                )
            })?;
        let background = Background {
            profile,
            noise: Kw::new(non_negative(
                "background.noise_kw",
                self.background.noise_kw,
            )?),
        };
        let appliances = self
            .appliances
            .into_iter()
            .enumerate()
            .map(|(i, a)| a.validate(i))
            .collect::<Result<_, _>>()?;
        let ev = self
            .ev
            .into_iter()
            .enumerate()
            .map(|(i, e)| e.validate(i))
            .collect::<Result<_, _>>()?;
        Ok(SimConfig {
            factor,
            start,
            seed: self.seed,
            initial_mode,
            outdoor: Celsius::new(finite("outdoor_c", self.outdoor_c)?),
            indoor: Celsius::new(finite("indoor_c", self.indoor_c)?),
            register: Kwh::new(non_negative("register_kwh", self.register_kwh)?),
            background,
            heat_pump: Kw::new(non_negative("heat_pump.max_kw", self.heat_pump.max_kw)?),
            appliances,
            ev,
            charger_limit: Ampere::new(positive("charger_limit_a", self.charger_limit_a)?),
            floors: floors(self.floors, eb)?,
            meter_topic: eb.topics.meter.clone(),
            tz,
        })
    }
}

impl RawAppliance {
    fn validate(self, i: usize) -> Result<Appliance, SimConfigError> {
        let key = |k: &str| format!("appliance[{i}].{k}");
        if self.minutes == 0 {
            return Err(invalid(key("minutes"), "must be above 0"));
        }
        if !(0..self.to_hour).contains(&self.from_hour) || self.to_hour > 24 {
            return Err(invalid(
                key("from_hour"),
                format!(
                    "{}..{} is not a window within 0..24",
                    self.from_hour, self.to_hour
                ),
            ));
        }
        Ok(Appliance {
            power: Kw::new(positive(&key("kw"), self.kw)?),
            duration: SignedDuration::from_mins(i64::from(self.minutes)),
            per_day: non_negative(&key("per_day"), self.per_day)?,
            from_hour: self.from_hour,
            to_hour: self.to_hour,
            name: self.name,
        })
    }
}

impl RawEv {
    fn validate(self, i: usize) -> Result<EvSession, SimConfigError> {
        let key = |k: &str| format!("ev[{i}].{k}");
        let from = self
            .from
            .parse::<Time>()
            .map_err(|e| invalid(key("from"), format!("{:?}: {e}", self.from)))?;
        Ok(EvSession {
            from,
            energy: Kwh::new(positive(&key("kwh"), self.kwh)?),
            power: Kw::new(positive(&key("kw"), self.kw)?),
        })
    }
}

/// One spec per energybrain zone, in its order; zones not listed get defaults.
fn floors(raw: Vec<RawFloor>, eb: &Config) -> Result<Vec<FloorSpec>, SimConfigError> {
    let mut given = BTreeMap::new();
    for (i, f) in raw.into_iter().enumerate() {
        if !eb.topics.zones.iter().any(|z| z.zone.as_str() == f.zone) {
            return Err(invalid(
                format!("floor[{i}].zone"),
                format!("{:?} is not a zone in the energybrain config", f.zone),
            ));
        }
        if given.contains_key(&f.zone) {
            return Err(invalid(
                format!("floor[{i}].zone"),
                format!("{:?} is listed twice", f.zone),
            ));
        }
        given.insert(f.zone.clone(), (i, f));
    }
    eb.topics
        .zones
        .iter()
        .zip(&eb.engine.zones)
        .map(|(topics, zone)| {
            let (i, f) = given
                .remove(topics.zone.as_str())
                .unwrap_or_else(|| (0, RawFloor::defaults(topics.zone.as_str())));
            let key = |k: &str| format!("floor[{i}].{k}");
            let setpoint = finite(&key("setpoint"), f.setpoint)?;
            if !(SETPOINT_MIN.value()..=SETPOINT_MAX.value()).contains(&setpoint) {
                return Err(invalid(
                    key("setpoint"),
                    format!("{setpoint} is outside {SETPOINT_MIN}–{SETPOINT_MAX}"),
                ));
            }
            Ok(FloorSpec {
                topics: topics.clone(),
                heater: match f.heater_kw {
                    Some(kw) => Kw::new(positive(&key("heater_kw"), kw)?),
                    None => zone.nominal_kw,
                },
                setpoint: Celsius::new(setpoint),
                capacity: HeatCapacity::new(positive(
                    &key("capacity_kwh_per_k"),
                    f.capacity_kwh_per_k,
                )?),
                loss: HeatLoss::new(positive(&key("loss_kw_per_k"), f.loss_kw_per_k)?),
                ambient: f
                    .ambient_c
                    .map(|c| finite(&key("ambient_c"), c).map(Celsius::new))
                    .transpose()?,
            })
        })
        .collect()
}

impl RawFloor {
    fn defaults(zone: &str) -> Self {
        Self {
            zone: zone.to_owned(),
            heater_kw: None,
            setpoint: default_setpoint(),
            capacity_kwh_per_k: default_capacity(),
            loss_kw_per_k: default_loss(),
            ambient_c: None,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use energybrain_core::charger::ChargerConfig;

    const EXAMPLE: &str = include_str!("../../../config/example.toml");

    pub(crate) fn eb() -> Config {
        energybrain_config::parse(EXAMPLE, &|_| None).unwrap()
    }

    pub(crate) const MINIMAL: &str = r#"start = "2026-12-01T16:00""#;

    fn err(text: &str) -> String {
        parse(text, &eb()).unwrap_err().to_string()
    }

    /// `MINIMAL` plus `extra`, expecting an error.
    fn err_with(extra: &str) -> String {
        err(&format!("{MINIMAL}\n{extra}"))
    }

    #[test]
    fn minimal_config_gets_defaults_for_everything() {
        let c = parse(MINIMAL, &eb()).unwrap();
        assert_eq!(c.factor.get(), 6);
        // 16:00 in Oslo in December is 15:00Z.
        assert_eq!(
            c.start,
            "2026-12-01T15:00:00Z".parse::<Timestamp>().unwrap()
        );
        assert_eq!(c.seed, 1);
        assert_eq!(c.initial_mode, None);
        assert_eq!(c.outdoor, Celsius::new(-5.0));
        assert_eq!(c.indoor, Celsius::new(20.0));
        assert_eq!(c.register, Kwh::new(10_000.0));
        assert_eq!(c.background.profile[18], Kw::new(1.6));
        assert_eq!(c.background.noise, Kw::new(0.15));
        assert_eq!(c.heat_pump, Kw::new(1.5));
        assert!(c.appliances.is_empty() && c.ev.is_empty());
        assert_eq!(c.meter_topic, "tibber");
        assert_eq!(c.tz.iana_name(), Some("Europe/Oslo"));
        // Every zone gets a floor, in the energybrain order, with its nominal power.
        let ids: Vec<_> = c.floors.iter().map(|f| f.topics.zone.as_str()).collect();
        assert_eq!(
            ids,
            [
                "bathroom", "laundry", "hallway", "guest_1", "guest_2", "basement"
            ]
        );
        let laundry = &c.floors[1];
        assert_eq!(laundry.heater, Kw::new(0.6));
        assert_eq!(laundry.setpoint, Celsius::new(22.0));
        assert_eq!(laundry.capacity, HeatCapacity::new(0.5));
        assert_eq!(laundry.loss, HeatLoss::new(0.08));
        assert_eq!(laundry.ambient, None);
        assert_eq!(
            laundry.topics.setpoint,
            "zwave/Laundry/Heating_cable/67/1/setpoint/1"
        );
    }

    #[test]
    fn full_config() {
        let text = r#"
            factor = 60
            start = "2026-07-01T08:30:00"
            seed = 9
            initial_mode = "Active"
            outdoor_c = 12.5
            indoor_c = 19
            register_kwh = 263743
            [background]
            profile_kw = [1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,2]
            noise_kw = 0
            [heat_pump]
            max_kw = 2
            [[appliance]]
            name = "oven"
            kw = 2.5
            minutes = 45
            per_day = 1
            from_hour = 16
            to_hour = 20
            [[ev]]
            from = "17:30"
            kwh = 20
            kw = 7.4
            [[floor]]
            zone = "basement"
            heater_kw = 0.8
            setpoint = 15
            capacity_kwh_per_k = 0.7
            loss_kw_per_k = 0.05
            ambient_c = 10
        "#;
        let c = parse(text, &eb()).unwrap();
        assert_eq!(c.factor.get(), 60);
        // Summer: UTC+2.
        assert_eq!(
            c.start,
            "2026-07-01T06:30:00Z".parse::<Timestamp>().unwrap()
        );
        assert_eq!(c.seed, 9);
        assert_eq!(c.initial_mode, Some(Mode::Active));
        assert_eq!(c.outdoor, Celsius::new(12.5));
        assert_eq!(c.indoor, Celsius::new(19.0));
        assert_eq!(c.register, Kwh::new(263_743.0));
        assert_eq!(c.background.profile[23], Kw::new(2.0));
        assert_eq!(c.background.noise, Kw::ZERO);
        assert_eq!(c.heat_pump, Kw::new(2.0));
        assert_eq!(
            c.appliances,
            [Appliance {
                name: "oven".into(),
                power: Kw::new(2.5),
                duration: SignedDuration::from_mins(45),
                per_day: 1.0,
                from_hour: 16,
                to_hour: 20,
            }]
        );
        assert_eq!(
            c.ev,
            [EvSession {
                from: Time::constant(17, 30, 0, 0),
                energy: Kwh::new(20.0),
                power: Kw::new(7.4),
            }]
        );
        let basement = &c.floors[5];
        assert_eq!(basement.heater, Kw::new(0.8));
        assert_eq!(basement.setpoint, Celsius::new(15.0));
        assert_eq!(basement.capacity, HeatCapacity::new(0.7));
        assert_eq!(basement.loss, HeatLoss::new(0.05));
        assert_eq!(basement.ambient, Some(Celsius::new(10.0)));
        // The others keep their defaults.
        assert_eq!(c.floors[0].heater, Kw::new(1.0));
    }

    #[test]
    fn rejects_bad_top_level_values() {
        assert!(err("").contains("missing field `start`"));
        assert!(err_with("bogus = 1").contains("unknown field `bogus`"));
        assert_eq!(
            err_with("factor = 0"),
            "factor: 0 is not between 1 and 3600"
        );
        assert_eq!(
            err_with("factor = 3601"),
            "factor: 3601 is not between 1 and 3600"
        );
        assert!(err(r#"start = "tomorrow""#).starts_with("start: \"tomorrow\": "));
        // The earliest civil time cannot be shifted to UTC in a zone ahead of it.
        assert!(err(r#"start = "-009999-01-01T00:00""#).starts_with("start: "));
        assert_eq!(err_with(r#"initial_mode = "on""#), "initial_mode: \"on\"");
        assert_eq!(
            err_with("outdoor_c = nan"),
            "outdoor_c: NaN is not a number"
        );
        assert_eq!(err_with("indoor_c = inf"), "indoor_c: inf is not a number");
        assert_eq!(
            err_with("register_kwh = -1"),
            "register_kwh: -1 is negative"
        );
        assert_eq!(
            err_with("[heat_pump]\nmax_kw = -1"),
            "heat_pump.max_kw: -1 is negative"
        );
    }

    #[test]
    fn rejects_bad_background() {
        assert_eq!(
            err_with("[background]\nprofile_kw = [1, 2]"),
            "background.profile_kw: has 2 values, needs 24"
        );
        assert_eq!(
            err_with("[background]\nprofile_kw = [1, -2]"),
            "background.profile_kw[1]: -2 is negative"
        );
        assert_eq!(
            err_with("[background]\nnoise_kw = -0.1"),
            "background.noise_kw: -0.1 is negative"
        );
    }

    fn appliance(fields: &str) -> String {
        err_with(&format!(
            "[[appliance]]\nname = \"x\"\nkw = 1\nminutes = 3\nper_day = 1\n{fields}"
        ))
    }

    #[test]
    fn rejects_bad_appliances() {
        assert_eq!(
            err_with("[[appliance]]\nname = \"x\"\nkw = 1\nminutes = 0\nper_day = 1"),
            "appliance[0].minutes: must be above 0"
        );
        assert_eq!(
            err_with("[[appliance]]\nname = \"x\"\nkw = 0\nminutes = 3\nper_day = 1"),
            "appliance[0].kw: 0 must be above 0"
        );
        assert_eq!(
            err_with("[[appliance]]\nname = \"x\"\nkw = 1\nminutes = 3\nper_day = -1"),
            "appliance[0].per_day: -1 is negative"
        );
        for window in [
            "from_hour = -1",
            "from_hour = 23\nto_hour = 23",
            "to_hour = 25",
        ] {
            assert!(
                appliance(window).starts_with("appliance[0].from_hour: "),
                "{window}"
            );
        }
    }

    #[test]
    fn rejects_bad_ev_sessions() {
        assert!(
            err_with("[[ev]]\nfrom = \"25:00\"\nkwh = 1\nkw = 1")
                .starts_with("ev[0].from: \"25:00\": ")
        );
        assert_eq!(
            err_with("[[ev]]\nfrom = \"17:00\"\nkwh = 0\nkw = 1"),
            "ev[0].kwh: 0 must be above 0"
        );
        assert_eq!(
            err_with("[[ev]]\nfrom = \"17:00\"\nkwh = 1\nkw = -1"),
            "ev[0].kw: -1 must be above 0"
        );
    }

    #[test]
    fn rejects_bad_floors() {
        assert_eq!(
            err_with("[[floor]]\nzone = \"attic\""),
            "floor[0].zone: \"attic\" is not a zone in the energybrain config"
        );
        assert_eq!(
            err_with("[[floor]]\nzone = \"bathroom\"\n[[floor]]\nzone = \"bathroom\""),
            "floor[1].zone: \"bathroom\" is listed twice"
        );
        let floor = |field: &str| err_with(&format!("[[floor]]\nzone = \"hallway\"\n{field}"));
        assert_eq!(
            floor("setpoint = 41"),
            "floor[0].setpoint: 41 is outside 5 °C–40 °C"
        );
        assert_eq!(
            floor("setpoint = nan"),
            "floor[0].setpoint: NaN is not a number"
        );
        assert_eq!(
            floor("heater_kw = 0"),
            "floor[0].heater_kw: 0 must be above 0"
        );
        assert_eq!(
            floor("capacity_kwh_per_k = 0"),
            "floor[0].capacity_kwh_per_k: 0 must be above 0"
        );
        assert_eq!(
            floor("loss_kw_per_k = 0"),
            "floor[0].loss_kw_per_k: 0 must be above 0"
        );
        assert_eq!(
            floor("ambient_c = nan"),
            "floor[0].ambient_c: NaN is not a number"
        );
    }

    #[test]
    fn the_dev_configs_are_valid_and_describe_the_real_house() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let dev = energybrain_config::load(&root.join("config/dev.toml"), &|_| None).unwrap();
        let sim = load(&root.join("config/sim.toml"), &dev).unwrap();
        assert_eq!(sim.factor.get(), 6);
        assert_eq!(sim.initial_mode, Some(Mode::Active));
        assert_eq!(dev.mqtt.host, "127.0.0.1");
        assert_eq!(dev.store_path, PathBuf::from("target/dev/energybrain.db"));
        // Same zones, topics and tariff as the real house, plus the simulated charger (its
        // section is commented out in example.toml).
        let real = eb();
        assert_eq!(dev.topics.zones, real.topics.zones);
        assert_eq!(dev.topics.meter, real.topics.meter);
        assert_eq!(dev.topics.prefix, real.topics.prefix);
        assert!(dev.topics.ev && dev.easee.is_some());
        assert_eq!(dev.engine.charger, Some(ChargerConfig::default()));
        assert_eq!(sim.charger_limit, Ampere::new(16.0));
        assert_eq!(dev.engine.zones, real.engine.zones);
        assert_eq!(dev.engine.tariff, real.engine.tariff);
        assert_eq!(dev.engine.target, real.engine.target);
    }

    #[test]
    fn load_reads_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sim.toml");
        let missing = load(&path, &eb()).unwrap_err().to_string();
        assert!(missing.starts_with("cannot read "), "{missing}");
        std::fs::write(&path, MINIMAL).unwrap();
        assert_eq!(load(&path, &eb()).unwrap().factor.get(), 6);
    }
}

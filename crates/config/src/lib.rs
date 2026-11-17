//! TOML configuration → validated, typed configuration.
//!
//! Parsing is pure ([`parse`]); [`load`] adds reading the file. Environment variables are
//! passed in as a lookup function so tests stay hermetic. Every validation error names the
//! offending key.

use std::collections::BTreeSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use energybrain_core::actuator::ActuatorConfig;
use energybrain_core::energy::MeterConfig;
use energybrain_core::engine::EngineConfig;
use energybrain_core::guard::GuardConfig;
use energybrain_core::ids::ZoneId;
use energybrain_core::tariff::{CapacityTariff, Step, TariffError};
use energybrain_core::time::LocalCalendar;
use energybrain_core::units::{Celsius, CelsiusDelta, Kr, Kw, Kwh};
use energybrain_core::zone::{ZoneConfig, ZoneSettings};
use jiff::SignedDuration;
use jiff::tz::TimeZone;
use serde::Deserialize;

/// Environment variable overriding `mqtt.username`.
pub const ENV_MQTT_USERNAME: &str = "ENERGYBRAIN_MQTT_USERNAME";
/// Environment variable overriding `mqtt.password`.
pub const ENV_MQTT_PASSWORD: &str = "ENERGYBRAIN_MQTT_PASSWORD";

/// Why the configuration was rejected.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
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

fn invalid(key: impl Into<String>, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key: key.into(),
        reason: reason.into(),
    }
}

/// Broker connection settings.
#[derive(Clone, PartialEq, Eq)]
pub struct MqttConfig {
    /// Broker host.
    pub host: String,
    /// Broker port.
    pub port: u16,
    /// MQTT client id.
    pub client_id: String,
    /// Keep-alive interval.
    pub keep_alive: SignedDuration,
    /// User name, if the broker needs one.
    pub username: Option<String>,
    /// Password, if the broker needs one.
    pub password: Option<String>,
}

impl fmt::Debug for MqttConfig {
    /// Never prints the password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MqttConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("client_id", &self.client_id)
            .field("keep_alive", &self.keep_alive)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The topics of one zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneTopics {
    /// The zone.
    pub zone: ZoneId,
    /// Reported setpoint; writes go to `<setpoint>/set`.
    pub setpoint: String,
    /// Reported power (W).
    pub power: String,
    /// Reported energy counter (kWh).
    pub energy: String,
}

/// MQTT topics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicConfig {
    /// HAN frames.
    pub meter: String,
    /// Prefix for our own status and control topics.
    pub prefix: String,
    /// Per-zone thermostat topics.
    pub zones: Vec<ZoneTopics>,
}

/// The validated configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Broker.
    pub mqtt: MqttConfig,
    /// Topics.
    pub topics: TopicConfig,
    /// HTTP listen address.
    pub http_listen: SocketAddr,
    /// SQLite database file.
    pub store_path: PathBuf,
    /// How long the 1-minute samples for the history graphs are kept.
    pub sample_retention: SignedDuration,
    /// Everything the engine needs.
    pub engine: EngineConfig,
}

/// Reads and validates a configuration file.
pub fn load(path: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    parse(&text, env)
}

/// Parses and validates configuration text.
pub fn parse(text: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
    let raw: Raw = toml::from_str(text)?;
    raw.validate(env)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(default = "default_timezone")]
    timezone: String,
    mqtt: RawMqtt,
    #[serde(default)]
    http: RawHttp,
    #[serde(default)]
    store: RawStore,
    meter: RawMeter,
    tariff: RawTariff,
    #[serde(default)]
    guard: RawGuard,
    #[serde(default)]
    actuator: RawActuator,
    #[serde(default)]
    zones: RawZoneSettings,
    #[serde(default)]
    thermostat: RawThermostat,
    #[serde(default, rename = "zone")]
    zone_list: Vec<RawZone>,
}

fn default_timezone() -> String {
    "Europe/Oslo".to_owned()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMqtt {
    host: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "default_client_id")]
    client_id: String,
    #[serde(default = "default_keep_alive")]
    keep_alive_secs: u32,
    #[serde(default = "default_prefix")]
    prefix: String,
    username: Option<String>,
    password: Option<String>,
}

fn default_port() -> u16 {
    1883
}
fn default_client_id() -> String {
    "energybrain".to_owned()
}
fn default_keep_alive() -> u32 {
    30
}
fn default_prefix() -> String {
    "energybrain".to_owned()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawHttp {
    listen: String,
}

impl Default for RawHttp {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".to_owned(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawStore {
    path: PathBuf,
    sample_retention_days: u32,
}

impl Default for RawStore {
    fn default() -> Self {
        Self {
            path: PathBuf::from("energybrain.db"),
            sample_retention_days: 90,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMeter {
    topic: String,
    #[serde(default = "default_power_window")]
    power_window_secs: u32,
    #[serde(default = "default_stale_after")]
    stale_after_secs: u32,
}

fn default_power_window() -> u32 {
    60
}
fn default_stale_after() -> u32 {
    120
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTariff {
    tiers: Vec<RawTier>,
    target_kw: f64,
    #[serde(default = "default_margin")]
    safety_margin_kwh: f64,
}

fn default_margin() -> f64 {
    0.3
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTier {
    to_kw: Option<f64>,
    price: f64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawGuard {
    release_margin_kwh: f64,
    min_shed_minutes: u32,
    restore_stagger_minutes: u32,
    settle_secs: u32,
}

impl Default for RawGuard {
    fn default() -> Self {
        Self {
            release_margin_kwh: 0.3,
            min_shed_minutes: 5,
            restore_stagger_minutes: 2,
            settle_secs: 90,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawActuator {
    deadband: f64,
    min_interval_secs: u32,
}

impl Default for RawActuator {
    fn default() -> Self {
        Self {
            deadband: 0.25,
            min_interval_secs: 60,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawZoneSettings {
    echo_tolerance: f64,
    override_minutes: u32,
    heating_threshold_kw: f64,
    learn_rate: f64,
}

impl Default for RawZoneSettings {
    fn default() -> Self {
        Self {
            echo_tolerance: 0.25,
            override_minutes: 120,
            heating_threshold_kw: 0.05,
            learn_rate: 0.2,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawThermostat {
    setpoint: String,
    power: String,
    energy: String,
}

impl Default for RawThermostat {
    /// Heatit Z-TRM3 via Z-Wave JS UI.
    fn default() -> Self {
        Self {
            setpoint: "67/1/setpoint/1".to_owned(),
            power: "50/1/value/66049".to_owned(),
            energy: "50/1/value/65537".to_owned(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawZone {
    id: String,
    name: Option<String>,
    device: String,
    priority: u8,
    #[serde(default = "yes")]
    shed_allowed: bool,
    #[serde(default = "default_shed_delta")]
    shed_delta: f64,
    #[serde(default = "default_min_setpoint")]
    min_setpoint: f64,
    #[serde(default = "default_max_setpoint")]
    max_setpoint: f64,
    #[serde(default = "default_nominal")]
    nominal_kw: f64,
    setpoint_topic: Option<String>,
    power_topic: Option<String>,
    energy_topic: Option<String>,
}

fn yes() -> bool {
    true
}
fn default_shed_delta() -> f64 {
    4.0
}
fn default_min_setpoint() -> f64 {
    5.0
}
fn default_max_setpoint() -> f64 {
    35.0
}
fn default_nominal() -> f64 {
    1.0
}

/// A finite value ≥ 0.
fn non_negative(key: &str, v: f64) -> Result<f64, ConfigError> {
    if v.is_finite() && v >= 0.0 {
        Ok(v)
    } else {
        Err(invalid(
            key,
            format!("must be a finite number ≥ 0, got {v}"),
        ))
    }
}

/// A finite value > 0.
fn positive(key: &str, v: f64) -> Result<f64, ConfigError> {
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        Err(invalid(
            key,
            format!("must be a finite number > 0, got {v}"),
        ))
    }
}

/// A topic without wildcards.
fn topic(key: &str, t: &str) -> Result<String, ConfigError> {
    if t.is_empty() || t.contains(['+', '#']) {
        Err(invalid(key, "must be a non-empty topic without wildcards"))
    } else {
        Ok(t.to_owned())
    }
}

fn secs(n: u32) -> SignedDuration {
    SignedDuration::from_secs(i64::from(n))
}

fn minutes(n: u32) -> SignedDuration {
    SignedDuration::from_mins(i64::from(n))
}

impl Raw {
    fn validate(self, env: &dyn Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
        let tz = TimeZone::get(&self.timezone).map_err(|e| invalid("timezone", e.to_string()))?;
        let mqtt = self.mqtt.validate(env)?;
        let http_listen = self
            .http
            .listen
            .parse()
            .map_err(|e| invalid("http.listen", format!("{e}")))?;
        if self.store.sample_retention_days == 0 {
            return Err(invalid("store.sample_retention_days", "must be at least 1"));
        }
        let sample_retention =
            SignedDuration::from_hours(i64::from(self.store.sample_retention_days) * 24);
        let tariff = validate_tariff(&self.tariff.tiers)?;
        let target_kw = self.tariff.target_kw;
        if !tariff
            .tiers()
            .any(|t| t.to.is_some_and(|to| (to.value() - target_kw).abs() < 1e-9))
        {
            return Err(invalid(
                "tariff.target_kw",
                format!("{target_kw} is not the upper bound (to_kw) of any tier"),
            ));
        }
        let safety_margin =
            non_negative("tariff.safety_margin_kwh", self.tariff.safety_margin_kwh)?;

        let g = &self.guard;
        let guard = GuardConfig {
            release_margin: Kwh::new(non_negative(
                "guard.release_margin_kwh",
                g.release_margin_kwh,
            )?),
            min_shed_duration: minutes(g.min_shed_minutes),
            restore_stagger: minutes(g.restore_stagger_minutes),
            settle: secs(g.settle_secs),
        };
        let actuator = ActuatorConfig {
            deadband: CelsiusDelta::new(non_negative("actuator.deadband", self.actuator.deadband)?),
            min_interval: secs(self.actuator.min_interval_secs),
        };
        let z = &self.zones;
        let learn_rate = positive("zones.learn_rate", z.learn_rate)?;
        if learn_rate > 1.0 {
            return Err(invalid("zones.learn_rate", "must be at most 1"));
        }
        let zone_settings = ZoneSettings {
            echo_tolerance: CelsiusDelta::new(non_negative(
                "zones.echo_tolerance",
                z.echo_tolerance,
            )?),
            override_duration: minutes(z.override_minutes),
            heating_threshold: Kw::new(non_negative(
                "zones.heating_threshold_kw",
                z.heating_threshold_kw,
            )?),
            learn_rate,
        };
        let meter = MeterConfig {
            power_window: secs(self.meter.power_window_secs.max(1)),
            stale_after: secs(self.meter.stale_after_secs),
        };
        let meter_topic = topic("meter.topic", &self.meter.topic)?;

        let mut ids = BTreeSet::new();
        let mut zones = Vec::new();
        let mut zone_topics = Vec::new();
        for (i, rz) in self.zone_list.into_iter().enumerate() {
            let (cfg, topics) = rz.validate(i, &self.thermostat)?;
            if !ids.insert(cfg.id.clone()) {
                return Err(invalid(
                    format!("zone[{i}].id"),
                    format!("duplicate id {:?}", cfg.id.as_str()),
                ));
            }
            zones.push(cfg);
            zone_topics.push(topics);
        }

        Ok(Config {
            topics: TopicConfig {
                meter: meter_topic,
                prefix: mqtt.1,
                zones: zone_topics,
            },
            mqtt: mqtt.0,
            http_listen,
            store_path: self.store.path,
            sample_retention,
            engine: EngineConfig {
                calendar: LocalCalendar::new(tz),
                tariff,
                target: Kw::new(target_kw),
                safety_margin: Kwh::new(safety_margin),
                meter,
                guard,
                actuator,
                zone_settings,
                zones,
            },
        })
    }
}

impl RawMqtt {
    fn validate(
        self,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<(MqttConfig, String), ConfigError> {
        if self.host.is_empty() {
            return Err(invalid("mqtt.host", "must not be empty"));
        }
        if self.client_id.is_empty() {
            return Err(invalid("mqtt.client_id", "must not be empty"));
        }
        let prefix = topic("mqtt.prefix", &self.prefix)?;
        let cfg = MqttConfig {
            host: self.host,
            port: self.port,
            client_id: self.client_id,
            keep_alive: secs(self.keep_alive_secs.max(5)),
            username: env(ENV_MQTT_USERNAME).or(self.username),
            password: env(ENV_MQTT_PASSWORD).or(self.password),
        };
        Ok((cfg, prefix))
    }
}

fn validate_tariff(tiers: &[RawTier]) -> Result<CapacityTariff, ConfigError> {
    let Some((top, bounded)) = tiers.split_last() else {
        return Err(invalid("tariff.tiers", "must not be empty"));
    };
    if top.to_kw.is_some() {
        return Err(invalid(
            format!("tariff.tiers[{}].to_kw", tiers.len() - 1),
            "the last tier must be open-ended (no to_kw)",
        ));
    }
    let mut steps = Vec::new();
    for (i, t) in bounded.iter().enumerate() {
        let Some(to) = t.to_kw else {
            return Err(invalid(
                format!("tariff.tiers[{i}].to_kw"),
                "required on all but the last tier",
            ));
        };
        let price = non_negative(&format!("tariff.tiers[{i}].price"), t.price)?;
        steps.push(Step {
            to: Kw::new(to),
            price: Kr::new(price),
        });
    }
    let top_price = non_negative(
        &format!("tariff.tiers[{}].price", tiers.len() - 1),
        top.price,
    )?;
    CapacityTariff::new(steps, Kr::new(top_price)).map_err(|e| {
        let index = match &e {
            TariffError::InvalidBound { index, .. } | TariffError::NotIncreasing { index, .. } => {
                *index
            }
        };
        invalid(format!("tariff.tiers[{index}].to_kw"), e.to_string())
    })
}

impl RawZone {
    fn validate(
        self,
        i: usize,
        th: &RawThermostat,
    ) -> Result<(ZoneConfig, ZoneTopics), ConfigError> {
        let key = |k: &str| format!("zone[{i}].{k}");
        let id = ZoneId::new(&self.id).map_err(|e| invalid(key("id"), e.to_string()))?;
        let device = topic(&key("device"), &self.device)?;
        let derived = |own: Option<String>, k: &str, suffix: &str| {
            topic(
                &key(k),
                &own.unwrap_or_else(|| format!("{device}/{suffix}")),
            )
        };
        let topics = ZoneTopics {
            zone: id.clone(),
            setpoint: derived(self.setpoint_topic, "setpoint_topic", &th.setpoint)?,
            power: derived(self.power_topic, "power_topic", &th.power)?,
            energy: derived(self.energy_topic, "energy_topic", &th.energy)?,
        };
        let min = non_negative(&key("min_setpoint"), self.min_setpoint)?;
        let max = non_negative(&key("max_setpoint"), self.max_setpoint)?;
        if min > max {
            return Err(invalid(
                key("min_setpoint"),
                format!("{min} is above max_setpoint {max}"),
            ));
        }
        let cfg = ZoneConfig {
            name: self.name.unwrap_or_else(|| self.id.clone()),
            id,
            priority: self.priority,
            shed_delta: CelsiusDelta::new(non_negative(&key("shed_delta"), self.shed_delta)?),
            min_setpoint: Celsius::new(min),
            max_setpoint: Celsius::new(max),
            shed_allowed: self.shed_allowed,
            nominal_kw: Kw::new(positive(&key("nominal_kw"), self.nominal_kw)?),
        };
        Ok((cfg, topics))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../config/example.toml");

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn ok(text: &str) -> Config {
        parse(text, &no_env).unwrap()
    }

    /// Parses `EXAMPLE` with one substitution and returns the error message.
    fn err_with(from: &str, to: &str) -> String {
        assert!(EXAMPLE.contains(from), "example lacks {from:?}");
        parse(&EXAMPLE.replacen(from, to, 1), &no_env)
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn the_example_is_valid_and_describes_the_house() {
        let c = ok(EXAMPLE);
        assert_eq!(c.mqtt.host, "192.168.1.172");
        assert_eq!(c.mqtt.port, 1883);
        assert_eq!(c.mqtt.username, None);
        assert_eq!(c.topics.meter, "tibber");
        assert_eq!(c.topics.prefix, "energybrain");
        assert_eq!(c.http_listen, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(c.store_path, PathBuf::from("/data/energybrain.db"));
        assert_eq!(c.sample_retention, SignedDuration::from_hours(90 * 24));
        let e = &c.engine;
        assert_eq!(e.calendar.time_zone().iana_name(), Some("Europe/Oslo"));
        assert_eq!(e.target, Kw::new(5.0));
        assert_eq!(e.safety_margin, Kwh::new(0.3));
        assert_eq!(e.tariff.len(), 15);
        assert_eq!(e.meter, MeterConfig::default());
        assert_eq!(e.guard, GuardConfig::default());
        assert_eq!(e.actuator, ActuatorConfig::default());
        assert_eq!(e.zone_settings, ZoneSettings::default());
        let order: Vec<_> = e
            .zones
            .iter()
            .map(|z| (z.id.as_str(), z.priority))
            .collect();
        assert_eq!(
            order,
            [
                ("bad", 1),
                ("vaskerom", 2),
                ("gang", 3),
                ("utleie_1", 4),
                ("utleie_2", 5),
                ("kjeller", 6)
            ]
        );
        let bad = &e.zones[0];
        assert_eq!(bad.name, "Bad");
        assert_eq!(bad.shed_delta, CelsiusDelta::new(4.0));
        assert!(bad.shed_allowed);
        assert_eq!(e.zones[1].nominal_kw, Kw::new(0.6));
        assert_eq!(
            c.topics.zones[0],
            ZoneTopics {
                zone: ZoneId::new("bad").unwrap(),
                setpoint: "zwave/Bad/Gulv_bad/67/1/setpoint/1".to_owned(),
                power: "zwave/Bad/Gulv_bad/50/1/value/66049".to_owned(),
                energy: "zwave/Bad/Gulv_bad/50/1/value/65537".to_owned(),
            }
        );
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let c = ok(r#"
            [mqtt]
            host = "broker"
            [meter]
            topic = "han"
            [tariff]
            tiers = [{ to_kw = 5, price = 1 }, { price = 2 }]
            target_kw = 5
            [[zone]]
            id = "z"
            device = "dev"
            priority = 1
            setpoint_topic = "own/setpoint"
        "#);
        assert_eq!(c.mqtt.client_id, "energybrain");
        assert_eq!(c.mqtt.keep_alive, SignedDuration::from_secs(30));
        assert_eq!(c.http_listen, "0.0.0.0:8080".parse().unwrap());
        assert_eq!(c.store_path, PathBuf::from("energybrain.db"));
        assert_eq!(c.sample_retention, SignedDuration::from_hours(90 * 24));
        assert_eq!(
            c.engine.calendar.time_zone().iana_name(),
            Some("Europe/Oslo")
        );
        let z = &c.engine.zones[0];
        assert_eq!(z.name, "z");
        assert_eq!(z.min_setpoint, Celsius::new(5.0));
        assert_eq!(z.max_setpoint, Celsius::new(35.0));
        assert_eq!(z.nominal_kw, Kw::new(1.0));
        assert_eq!(c.topics.zones[0].setpoint, "own/setpoint");
        assert_eq!(c.topics.zones[0].power, "dev/50/1/value/66049");
    }

    #[test]
    fn environment_overrides_credentials_and_debug_hides_the_password() {
        let vars = [(ENV_MQTT_USERNAME, "energy"), (ENV_MQTT_PASSWORD, "s3cret")];
        let env = |k: &str| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        };
        let text = EXAMPLE.replace("# username = \"\"", "username = \"file-user\"");
        let c = parse(&text, &env).unwrap();
        assert_eq!(c.mqtt.username.as_deref(), Some("energy"));
        assert_eq!(c.mqtt.password.as_deref(), Some("s3cret"));
        let dbg = format!("{:?}", c.mqtt);
        assert!(
            dbg.contains("<redacted>") && !dbg.contains("s3cret"),
            "{dbg}"
        );
        let file_only = parse(&text, &no_env).unwrap();
        assert_eq!(file_only.mqtt.username.as_deref(), Some("file-user"));
        assert!(format!("{:?}", file_only.mqtt).contains("password: None"));
    }

    #[test]
    fn load_reads_files_and_reports_missing_ones() {
        let dir = std::env::temp_dir().join(format!("energybrain-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.toml");
        std::fs::write(&path, EXAMPLE).unwrap();
        assert!(load(&path, &no_env).is_ok());
        let missing = dir.join("missing.toml");
        let err = load(&missing, &no_env).unwrap_err().to_string();
        assert!(
            err.starts_with("cannot read ") && err.contains("missing.toml"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn syntax_and_unknown_keys_are_rejected_with_their_location() {
        let err = err_with("keep_alive_secs = 30", "keep_alive = 30");
        assert!(err.contains("unknown field `keep_alive`"), "{err}");
        let err = err_with("port = 1883", "port = \"x\"");
        assert!(err.contains("port"), "{err}");
    }

    /// (text in the example, replacement, expected error prefix)
    const INVALID: &[(&str, &str, &str)] = &[
        (
            "timezone = \"Europe/Oslo\"",
            "timezone = \"Mars/Base\"",
            "timezone: ",
        ),
        (
            "host = \"192.168.1.172\"",
            "host = \"\"",
            "mqtt.host: must not be empty",
        ),
        (
            "client_id = \"energybrain\"",
            "client_id = \"\"",
            "mqtt.client_id: must not be empty",
        ),
        (
            "client_id = \"energybrain\"",
            "client_id = \"e\"\nprefix = \"a/#\"",
            "mqtt.prefix: ",
        ),
        (
            "listen = \"0.0.0.0:8080\"",
            "listen = \"nowhere\"",
            "http.listen: ",
        ),
        ("topic = \"tibber\"", "topic = \"\"", "meter.topic: "),
        (
            "sample_retention_days = 90",
            "sample_retention_days = 0",
            "store.sample_retention_days: must be at least 1",
        ),
        (
            "target_kw = 5",
            "target_kw = 6",
            "tariff.target_kw: 6 is not the upper bound",
        ),
        (
            "safety_margin_kwh = 0.3",
            "safety_margin_kwh = -1",
            "tariff.safety_margin_kwh: must be a finite number ≥ 0",
        ),
        (
            "release_margin_kwh = 0.3",
            "release_margin_kwh = nan",
            "guard.release_margin_kwh: ",
        ),
        ("deadband = 0.25", "deadband = -0.1", "actuator.deadband: "),
        (
            "learn_rate = 0.2",
            "learn_rate = 0",
            "zones.learn_rate: must be a finite number > 0",
        ),
        (
            "learn_rate = 0.2",
            "learn_rate = 1.5",
            "zones.learn_rate: must be at most 1",
        ),
        (
            "echo_tolerance = 0.25",
            "echo_tolerance = -1",
            "zones.echo_tolerance: ",
        ),
        (
            "heating_threshold_kw = 0.05",
            "heating_threshold_kw = inf",
            "zones.heating_threshold_kw: must be a finite number ≥ 0, got inf",
        ),
        (
            "{ to_kw = 2, price = 131 }",
            "{ price = 131 }",
            "tariff.tiers[0].to_kw: required on all but the last tier",
        ),
        (
            "{ price = 21473 }",
            "{ to_kw = 600, price = 21473 }",
            "tariff.tiers[14].to_kw: the last tier must be open-ended",
        ),
        (
            "{ to_kw = 5, price = 233 }",
            "{ to_kw = 1, price = 233 }",
            "tariff.tiers[1].to_kw: tier 1: upper bound 1 kW must be greater",
        ),
        (
            "{ to_kw = 2, price = 131 }",
            "{ to_kw = -2, price = 131 }",
            "tariff.tiers[0].to_kw: tier 0: upper bound -2 kW must be a positive",
        ),
        (
            "{ to_kw = 2, price = 131 }",
            "{ to_kw = 2, price = -1 }",
            "tariff.tiers[0].price: ",
        ),
        (
            "{ price = 21473 }",
            "{ price = -1 }",
            "tariff.tiers[14].price: ",
        ),
        (
            "id = \"bad\"",
            "id = \"Bad\"",
            "zone[0].id: zone id \"Bad\" may only contain",
        ),
        (
            "id = \"vaskerom\"",
            "id = \"bad\"",
            "zone[1].id: duplicate id \"bad\"",
        ),
        (
            "device = \"zwave/Bad/Gulv_bad\"",
            "device = \"\"",
            "zone[0].device: ",
        ),
        (
            "device = \"zwave/Bad/Gulv_bad\"",
            "device = \"d\"\npower_topic = \"x/+\"",
            "zone[0].power_topic: ",
        ),
        (
            "priority = 1",
            "priority = 1\nmin_setpoint = 30\nmax_setpoint = 20",
            "zone[0].min_setpoint: 30 is above max_setpoint 20",
        ),
        (
            "priority = 1",
            "priority = 1\nmax_setpoint = -5",
            "zone[0].max_setpoint: ",
        ),
        (
            "priority = 1",
            "priority = 1\nmin_setpoint = -5",
            "zone[0].min_setpoint: ",
        ),
        (
            "priority = 1",
            "priority = 1\nshed_delta = -4",
            "zone[0].shed_delta: ",
        ),
        (
            "nominal_kw = 0.6",
            "nominal_kw = 0",
            "zone[1].nominal_kw: must be a finite number > 0",
        ),
        (
            "priority = 1",
            "priority = 1\nsetpoint_topic = \"\"",
            "zone[0].setpoint_topic: ",
        ),
        (
            "priority = 1",
            "priority = 1\nenergy_topic = \"#\"",
            "zone[0].energy_topic: ",
        ),
    ];

    #[test]
    fn each_invalid_value_names_its_key() {
        for &(from, to, expected) in INVALID {
            let err = err_with(from, to);
            assert!(err.starts_with(expected), "{from} → {to}: got {err:?}");
        }
    }

    #[test]
    fn empty_tier_table_is_rejected() {
        let text = r#"
            [mqtt]
            host = "b"
            [meter]
            topic = "han"
            [tariff]
            tiers = []
            target_kw = 5
        "#;
        assert_eq!(
            parse(text, &no_env).unwrap_err().to_string(),
            "tariff.tiers: must not be empty"
        );
    }
}

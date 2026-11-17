//! Pure translation between MQTT messages and engine inputs/outputs.
//!
//! Inbound: HAN frames on the meter topic, plain-number thermostat values (Z-Wave JS UI
//! "value only"), and our own Easy MQTT control topics (`<prefix>/…/set`).
//! Outbound: setpoint writes to `<setpoint topic>/set`, and retained status topics for
//! HomeKit. Retained topics are only re-published when their payload changes, and all of
//! them again after a reconnect.

use std::collections::{BTreeMap, HashMap};

use energybrain_config::TopicConfig;
use energybrain_core::guard::GuardStatus;
use energybrain_core::ids::ZoneId;
use energybrain_core::io::{Input, Output, Status};
use energybrain_core::mode::Mode;
use energybrain_core::units::{Celsius, Kw, Kwh};
use energybrain_han::HanError;
use jiff::Timestamp;
use jiff::tz::TimeZone;

/// A message to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publish {
    /// Topic.
    pub topic: String,
    /// Payload (UTF-8 text for everything we send).
    pub payload: String,
    /// Whether the broker should retain it.
    pub retain: bool,
}

/// What an inbound message meant.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded {
    /// Inputs for the engine.
    Inputs(Vec<Input>),
    /// Not for us (or deliberately ignored, like the Pulse's own JSON status).
    Ignored,
    /// For us, but malformed. Worth a warning.
    Invalid {
        /// The topic.
        topic: String,
        /// What was wrong.
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZoneValue {
    Setpoint,
    Power,
    Energy,
}

/// Translates between MQTT and the engine. Holds only the retained-topic cache.
#[derive(Debug, Clone)]
pub struct Translator {
    meter: String,
    prefix: String,
    tz: TimeZone,
    zone_topics: HashMap<String, (ZoneId, ZoneValue)>,
    setpoint_topics: HashMap<ZoneId, String>,
    zones: Vec<ZoneId>,
    retained: BTreeMap<String, String>,
}

impl Translator {
    /// A translator for the configured topics. `tz` resolves the meter's local clock.
    pub fn new(topics: &TopicConfig, tz: TimeZone) -> Self {
        let mut zone_topics = HashMap::new();
        let mut setpoint_topics = HashMap::new();
        for z in &topics.zones {
            zone_topics.insert(z.setpoint.clone(), (z.zone.clone(), ZoneValue::Setpoint));
            zone_topics.insert(z.power.clone(), (z.zone.clone(), ZoneValue::Power));
            zone_topics.insert(z.energy.clone(), (z.zone.clone(), ZoneValue::Energy));
            setpoint_topics.insert(z.zone.clone(), z.setpoint.clone());
        }
        Self {
            meter: topics.meter.clone(),
            prefix: topics.prefix.clone(),
            tz,
            zone_topics,
            setpoint_topics,
            zones: topics.zones.iter().map(|z| z.zone.clone()).collect(),
            retained: BTreeMap::new(),
        }
    }

    /// Topic filters to subscribe to.
    pub fn subscriptions(&self) -> Vec<String> {
        let mut subs = vec![self.meter.clone()];
        let mut zone_topics: Vec<String> = self.zone_topics.keys().cloned().collect();
        zone_topics.sort();
        subs.extend(zone_topics);
        let p = &self.prefix;
        subs.push(format!("{p}/mode/set"));
        subs.push(format!("{p}/active/set"));
        subs.push(format!("{p}/zone/+/shed_allowed/set"));
        subs
    }

    /// Interprets one inbound message received at `now`.
    pub fn decode(&self, topic: &str, payload: &[u8], now: Timestamp) -> Decoded {
        let invalid = |reason: String| Decoded::Invalid {
            topic: topic.to_owned(),
            reason,
        };
        if topic == self.meter {
            return self.decode_meter(payload, now).unwrap_or_else(invalid);
        }
        let text = String::from_utf8_lossy(payload);
        let text = text.trim();
        if let Some((zone, kind)) = self.zone_topics.get(topic) {
            let Some(v) = number(text) else {
                return invalid(format!("not a number: {text:?}"));
            };
            let zone = zone.clone();
            let input = match kind {
                ZoneValue::Setpoint => Input::ThermostatSetpoint {
                    zone,
                    at: now,
                    value: Celsius::new(v),
                },
                ZoneValue::Power => Input::ZonePower {
                    zone,
                    at: now,
                    power: Kw::new(v / 1000.0),
                },
                ZoneValue::Energy => Input::ZoneEnergy {
                    zone,
                    at: now,
                    total: Kwh::new(v),
                },
            };
            return Decoded::Inputs(vec![input]);
        }
        let Some(rest) = topic
            .strip_prefix(self.prefix.as_str())
            .and_then(|r| r.strip_prefix('/'))
            .and_then(|r| r.strip_suffix("/set"))
        else {
            return Decoded::Ignored;
        };
        let control = match rest.split('/').collect::<Vec<_>>().as_slice() {
            ["mode"] => Mode::parse(text)
                .map(|mode| Input::SetMode { at: now, mode })
                .ok_or_else(|| format!("unknown mode {text:?}")),
            ["active"] => switch(text).map(|on| Input::SetMode {
                at: now,
                mode: if on { Mode::Active } else { Mode::Observe },
            }),
            ["zone", id, "shed_allowed"] => match self.zones.iter().find(|z| z.as_str() == *id) {
                Some(zone) => switch(text).map(|allowed| Input::SetShedAllowed {
                    zone: zone.clone(),
                    allowed,
                }),
                None => Err(format!("unknown zone {id:?}")),
            },
            _ => return Decoded::Ignored,
        };
        control.map_or_else(invalid, |input| Decoded::Inputs(vec![input]))
    }

    fn decode_meter(&self, payload: &[u8], now: Timestamp) -> Result<Decoded, String> {
        let reading = match energybrain_han::parse(payload) {
            Ok(r) => r,
            Err(HanError::NotHdlc) => return Ok(Decoded::Ignored),
            Err(e) => return Err(e.to_string()),
        };
        let mut inputs = vec![Input::MeterPower {
            at: now,
            power: reading.import_power,
        }];
        if let Some(reg) = reading.register {
            let at = reg.captured_at(&self.tz).map_err(|e| e.to_string())?;
            inputs.push(Input::MeterCumulative {
                at,
                total: reg.import,
            });
        }
        Ok(Decoded::Inputs(inputs))
    }

    /// What to publish for an engine output.
    pub fn encode(&mut self, output: &Output) -> Vec<Publish> {
        match output {
            Output::WriteSetpoint { zone, value, .. } => self
                .setpoint_topics
                .get(zone)
                .map(|t| Publish {
                    topic: format!("{t}/set"),
                    payload: value.value().to_string(),
                    retain: false,
                })
                .into_iter()
                .collect(),
            Output::PublishStatus(status) => self.status(status),
            Output::WouldWrite { .. }
            | Output::PersistHour(_)
            | Output::PersistZoneWrite { .. }
            | Output::PersistZoneBase { .. }
            | Output::PersistOverride { .. }
            | Output::PersistZoneHour { .. }
            | Output::PersistZoneStats { .. }
            | Output::PersistMode(_)
            | Output::PersistShedAllowed { .. }
            | Output::PersistEvent(_) => Vec::new(),
        }
    }

    /// Everything retained, for republishing after a reconnect.
    pub fn retained(&self) -> Vec<Publish> {
        self.retained
            .iter()
            .map(|(topic, payload)| Publish {
                topic: topic.clone(),
                payload: payload.clone(),
                retain: true,
            })
            .collect()
    }

    fn status(&mut self, s: &Status) -> Vec<Publish> {
        let p = self.prefix.clone();
        let flag = |b: bool| if b { "1" } else { "0" }.to_owned();
        let guard_active = match s.guard {
            GuardStatus::Shedding | GuardStatus::LimitUnreachable => true,
            GuardStatus::Idle | GuardStatus::MeterStale | GuardStatus::Disabled => false,
        };
        let mut topics = vec![
            (format!("{p}/mode"), s.mode.as_str().to_owned()),
            (format!("{p}/active"), flag(s.mode == Mode::Active)),
            (format!("{p}/peakguard/active"), flag(guard_active)),
        ];
        for z in &s.zones {
            topics.push((format!("{p}/zone/{}/shed", z.id), flag(z.shed)));
            topics.push((
                format!("{p}/zone/{}/shed_allowed", z.id),
                flag(z.shed_allowed),
            ));
        }
        // Serializing plain data structs to a String cannot fail.
        #[allow(clippy::expect_used)]
        let json = serde_json::to_string(s).expect("status serializes");
        topics.push((format!("{p}/status"), json));
        let mut out = Vec::new();
        for (topic, payload) in topics {
            if self.retained.get(&topic) == Some(&payload) {
                continue;
            }
            self.retained.insert(topic.clone(), payload.clone());
            out.push(Publish {
                topic,
                payload,
                retain: true,
            });
        }
        out
    }
}

fn number(text: &str) -> Option<f64> {
    text.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn switch(text: &str) -> Result<bool, String> {
    match text.to_ascii_lowercase().as_str() {
        "1" | "true" | "on" => Ok(true),
        "0" | "false" | "off" => Ok(false),
        _ => Err(format!("expected 1/0, true/false or on/off, got {text:?}")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use energybrain_config::ZoneTopics;
    use energybrain_core::energy::{Freshness, HourEnergy, Quality};
    use energybrain_core::io::{Event, EventKind, ZoneStatus};
    use energybrain_core::peaks::{HourLimit, LimitBasis};
    use energybrain_core::tariff::{Tier, TierIndex};
    use energybrain_core::time::HourStart;
    use energybrain_core::units::Kr;

    use super::*;

    pub(crate) fn topics() -> TopicConfig {
        let zone = |id: &str, dev: &str| ZoneTopics {
            zone: ZoneId::new(id).unwrap(),
            setpoint: format!("zwave/{dev}/67/1/setpoint/1"),
            power: format!("zwave/{dev}/50/1/value/66049"),
            energy: format!("zwave/{dev}/50/1/value/65537"),
        };
        TopicConfig {
            meter: "tibber".to_owned(),
            prefix: "energybrain".to_owned(),
            zones: vec![
                zone("bathroom", "Bathroom/Floor_bathroom"),
                zone("hallway", "Hallway/Floor_hallway"),
            ],
        }
    }

    pub(crate) fn translator() -> Translator {
        Translator::new(&topics(), TimeZone::get("Europe/Oslo").unwrap())
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/../../tests/fixtures/han/{name}.hex",
            env!("CARGO_MANIFEST_DIR")
        );
        let hex = std::fs::read_to_string(path).unwrap();
        let hex = hex.trim();
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    const NOW: &str = "2026-11-15T21:00:10Z";

    fn decode(topic: &str, payload: &[u8]) -> Decoded {
        translator().decode(topic, payload, ts(NOW))
    }

    fn bathroom() -> ZoneId {
        ZoneId::new("bathroom").unwrap()
    }

    #[test]
    fn subscribes_to_meter_zones_and_controls() {
        let subs = translator().subscriptions();
        assert_eq!(subs[0], "tibber");
        assert!(subs.contains(&"zwave/Bathroom/Floor_bathroom/67/1/setpoint/1".to_owned()));
        assert!(subs.contains(&"zwave/Hallway/Floor_hallway/50/1/value/65537".to_owned()));
        assert_eq!(
            &subs[subs.len() - 3..],
            [
                "energybrain/mode/set",
                "energybrain/active/set",
                "energybrain/zone/+/shed_allowed/set"
            ]
        );
    }

    #[test]
    fn han_frames_become_power_and_register_inputs() {
        assert_eq!(
            decode("tibber", &fixture("kaifa_ma304h3e_list1")),
            Decoded::Inputs(vec![Input::MeterPower {
                at: ts(NOW),
                power: Kw::new(6.371),
            }])
        );
        assert_eq!(
            decode("tibber", &fixture("kaifa_ma304h3e_list3")),
            Decoded::Inputs(vec![
                Input::MeterPower {
                    at: ts(NOW),
                    power: Kw::new(5.616),
                },
                Input::MeterCumulative {
                    at: ts("2026-11-15T21:00:00Z"),
                    total: Kwh::new(263_743.035),
                },
            ])
        );
        assert_eq!(decode("tibber", br#"{"status":{}}"#), Decoded::Ignored);
        let mut corrupt = fixture("kaifa_ma304h3e_list1");
        corrupt[20] ^= 0xff;
        assert_eq!(
            decode("tibber", &corrupt),
            Decoded::Invalid {
                topic: "tibber".to_owned(),
                reason: "frame checksum mismatch".to_owned(),
            }
        );
    }

    #[test]
    fn register_with_an_unresolvable_clock_is_invalid() {
        // Fixed UTC-2 zone: the meter's year-9999 clock cannot become an instant.
        let mut t = Translator::new(
            &topics(),
            TimeZone::fixed(jiff::tz::Offset::from_hours(-2).unwrap()),
        );
        t.meter = "m".to_owned();
        let mut f = fixture("kaifa_ma304h3e_list3");
        // Patch the list-3 clock's year to 9999 and the month/day to 12-31 23:59.
        let at = f
            .windows(4)
            .rposition(|w| w == [0x09, 0x0c, 0x07, 0xea])
            .unwrap();
        f[at + 2..at + 10].copy_from_slice(&[0x27, 0x0f, 12, 31, 4, 23, 59, 59]);
        let n = f.len();
        let fcs = crc(&f[1..n - 3]).to_le_bytes();
        f[n - 3] = fcs[0];
        f[n - 2] = fcs[1];
        assert!(
            matches!(t.decode("m", &f, ts(NOW)), Decoded::Invalid { reason, .. } if reason.starts_with("bad date-time"))
        );
    }

    fn crc(data: &[u8]) -> u16 {
        let mut c: u16 = 0xffff;
        for &b in data {
            c ^= u16::from(b);
            for _ in 0..8 {
                c = if c & 1 == 1 {
                    (c >> 1) ^ 0x8408
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    #[test]
    fn thermostat_values_are_plain_numbers() {
        assert_eq!(
            decode("zwave/Bathroom/Floor_bathroom/67/1/setpoint/1", b"26.5"),
            Decoded::Inputs(vec![Input::ThermostatSetpoint {
                zone: bathroom(),
                at: ts(NOW),
                value: Celsius::new(26.5),
            }])
        );
        assert_eq!(
            decode("zwave/Bathroom/Floor_bathroom/50/1/value/66049", b" 1500\n"),
            Decoded::Inputs(vec![Input::ZonePower {
                zone: bathroom(),
                at: ts(NOW),
                power: Kw::new(1.5),
            }])
        );
        assert_eq!(
            decode("zwave/Bathroom/Floor_bathroom/50/1/value/65537", b"9558.2"),
            Decoded::Inputs(vec![Input::ZoneEnergy {
                zone: bathroom(),
                at: ts(NOW),
                total: Kwh::new(9558.2),
            }])
        );
        assert_eq!(
            decode("zwave/Bathroom/Floor_bathroom/67/1/setpoint/1", b"NaN"),
            Decoded::Invalid {
                topic: "zwave/Bathroom/Floor_bathroom/67/1/setpoint/1".to_owned(),
                reason: "not a number: \"NaN\"".to_owned(),
            }
        );
        assert_eq!(
            decode("zwave/Living/Ceiling_light/50/0/value/66050", b"1"),
            Decoded::Ignored
        );
    }

    #[test]
    fn easy_mqtt_controls() {
        let mode = |m| {
            Decoded::Inputs(vec![Input::SetMode {
                at: ts(NOW),
                mode: m,
            }])
        };
        assert_eq!(
            decode("energybrain/mode/set", b"active"),
            mode(Mode::Active)
        );
        assert_eq!(decode("energybrain/mode/set", b"OFF"), mode(Mode::Off));
        assert_eq!(decode("energybrain/active/set", b"1"), mode(Mode::Active));
        assert_eq!(
            decode("energybrain/active/set", b"false"),
            mode(Mode::Observe)
        );
        assert_eq!(
            decode("energybrain/zone/bathroom/shed_allowed/set", b"off"),
            Decoded::Inputs(vec![Input::SetShedAllowed {
                zone: bathroom(),
                allowed: false,
            }])
        );
        assert_eq!(
            decode("energybrain/zone/hallway/shed_allowed/set", b"ON"),
            Decoded::Inputs(vec![Input::SetShedAllowed {
                zone: ZoneId::new("hallway").unwrap(),
                allowed: true,
            }])
        );
        let invalid = |topic: &str, reason: &str| Decoded::Invalid {
            topic: topic.to_owned(),
            reason: reason.to_owned(),
        };
        assert_eq!(
            decode("energybrain/mode/set", b"turbo"),
            invalid("energybrain/mode/set", "unknown mode \"turbo\"")
        );
        assert_eq!(
            decode("energybrain/active/set", b"maybe"),
            invalid(
                "energybrain/active/set",
                "expected 1/0, true/false or on/off, got \"maybe\""
            )
        );
        assert_eq!(
            decode("energybrain/zone/sauna/shed_allowed/set", b"1"),
            invalid(
                "energybrain/zone/sauna/shed_allowed/set",
                "unknown zone \"sauna\""
            )
        );
        for ignored in [
            "energybrain/mode",
            "energybrain/other/set",
            "energybrainx/mode/set",
            "other/mode/set",
        ] {
            assert_eq!(decode(ignored, b"active"), Decoded::Ignored, "{ignored}");
        }
    }

    pub(crate) fn status(mode: Mode, guard: GuardStatus, shed: bool) -> Status {
        let hour = HourStart::containing(ts(NOW));
        Status {
            at: ts(NOW),
            mode,
            guard,
            hour,
            used: Kwh::ZERO,
            projected: None,
            power: None,
            freshness: Freshness::NoData,
            meter_last_seen: None,
            limit: HourLimit {
                allowed: Kwh::new(5.0),
                effective: Kwh::new(4.7),
                basis: LimitBasis::TierBound,
            },
            top_days: vec![],
            top_average: Kw::ZERO,
            tier: Tier {
                index: TierIndex(0),
                from: Kw::ZERO,
                to: Some(Kw::new(2.0)),
                price: Kr::new(131.0),
            },
            zones: vec![ZoneStatus {
                id: bathroom(),
                name: "Bathroom".to_owned(),
                base: None,
                reported: None,
                decision: None,
                reasons: vec![],
                shed,
                shed_allowed: true,
                override_until: None,
                power: None,
                heating_kw: None,
            }],
        }
    }

    fn topics_of(p: &[Publish]) -> Vec<(&str, &str)> {
        p.iter()
            .filter(|p| !p.topic.ends_with("/status"))
            .map(|p| (p.topic.as_str(), p.payload.as_str()))
            .collect()
    }

    #[test]
    fn setpoint_writes_go_to_the_set_topic() {
        let mut t = translator();
        let write = |zone: ZoneId, v: f64| Output::WriteSetpoint {
            zone,
            value: Celsius::new(v),
            reasons: vec![],
        };
        assert_eq!(
            t.encode(&write(bathroom(), 22.5)),
            [Publish {
                topic: "zwave/Bathroom/Floor_bathroom/67/1/setpoint/1/set".to_owned(),
                payload: "22.5".to_owned(),
                retain: false,
            }]
        );
        assert_eq!(t.encode(&write(bathroom(), 18.0))[0].payload, "18");
        assert_eq!(
            t.encode(&write(ZoneId::new("ghost").unwrap(), 18.0)),
            Vec::<Publish>::new()
        );
    }

    #[test]
    fn status_becomes_retained_homekit_topics_published_on_change() {
        let mut t = translator();
        let first = t.encode(&Output::PublishStatus(Box::new(status(
            Mode::Active,
            GuardStatus::Shedding,
            true,
        ))));
        assert!(first.iter().all(|p| p.retain));
        assert_eq!(
            topics_of(&first),
            [
                ("energybrain/mode", "active"),
                ("energybrain/active", "1"),
                ("energybrain/peakguard/active", "1"),
                ("energybrain/zone/bathroom/shed", "1"),
                ("energybrain/zone/bathroom/shed_allowed", "1"),
            ]
        );
        let json = &first.last().unwrap();
        assert_eq!(json.topic, "energybrain/status");
        assert!(
            json.payload
                .starts_with(r#"{"at":"2026-11-15T21:00:10Z","mode":"active""#)
        );
        // Same status again: nothing new.
        let again = t.encode(&Output::PublishStatus(Box::new(status(
            Mode::Active,
            GuardStatus::Shedding,
            true,
        ))));
        assert_eq!(again, Vec::<Publish>::new());
        // Changes only.
        let changed = t.encode(&Output::PublishStatus(Box::new(status(
            Mode::Observe,
            GuardStatus::Idle,
            false,
        ))));
        assert_eq!(
            topics_of(&changed),
            [
                ("energybrain/mode", "observe"),
                ("energybrain/active", "0"),
                ("energybrain/peakguard/active", "0"),
                ("energybrain/zone/bathroom/shed", "0"),
            ]
        );
        // After a reconnect everything retained is sent again.
        let all = t.retained();
        assert_eq!(all.len(), 6);
        assert!(
            all.iter()
                .any(|p| p.topic == "energybrain/mode" && p.payload == "observe")
        );
    }

    #[test]
    fn guard_states_map_to_the_contact_sensor() {
        let active = |g| {
            let mut t = translator();
            let out = t.encode(&Output::PublishStatus(Box::new(status(
                Mode::Observe,
                g,
                false,
            ))));
            out.iter()
                .find(|p| p.topic == "energybrain/peakguard/active")
                .unwrap()
                .payload
                .clone()
        };
        assert_eq!(active(GuardStatus::LimitUnreachable), "1");
        assert_eq!(active(GuardStatus::MeterStale), "0");
        assert_eq!(active(GuardStatus::Disabled), "0");
    }

    #[test]
    fn persistence_outputs_publish_nothing() {
        let mut t = translator();
        let hour = HourStart::containing(ts(NOW));
        let outputs = [
            Output::WouldWrite {
                zone: bathroom(),
                value: Celsius::new(1.0),
                reasons: vec![],
            },
            Output::PersistHour(HourEnergy {
                hour,
                kwh: Kwh::ZERO,
                quality: Quality::Measured,
            }),
            Output::PersistZoneWrite {
                zone: bathroom(),
                value: Celsius::new(1.0),
                at: ts(NOW),
            },
            Output::PersistZoneBase {
                zone: bathroom(),
                value: Celsius::new(1.0),
            },
            Output::PersistOverride {
                zone: bathroom(),
                manual: None,
            },
            Output::PersistZoneHour {
                zone: bathroom(),
                hour,
                kwh: Kwh::ZERO,
            },
            Output::PersistZoneStats {
                zone: bathroom(),
                heating_kw: Kw::ZERO,
            },
            Output::PersistMode(Mode::Off),
            Output::PersistShedAllowed {
                zone: bathroom(),
                allowed: true,
            },
            Output::PersistEvent(Event {
                at: ts(NOW),
                mode: Mode::Off,
                kind: EventKind::OverrideExpired { zone: bathroom() },
            }),
        ];
        for o in &outputs {
            assert!(t.encode(o).is_empty(), "{o:?}");
        }
    }
}

//! Prometheus text exposition, rendered from the latest status plus write counters.
//!
//! Metric names are fixed by PLAN.md so a dashboard can be built against them later.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use energybrain_core::charger::ChargerStatus;
use energybrain_core::ids::ZoneId;
use energybrain_core::io::{Output, Status};
use energybrain_core::mode::Mode;

/// Write counters; everything else comes from the status at render time.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Metrics {
    writes: BTreeMap<ZoneId, u64>,
    would_writes: BTreeMap<ZoneId, u64>,
    charger_writes: u64,
    charger_would_writes: u64,
}

impl Metrics {
    /// Counts the writes and would-writes among the engine's outputs.
    pub fn record(&mut self, output: &Output) {
        match output {
            Output::WriteSetpoint { zone, .. } => {
                *self.writes.entry(zone.clone()).or_default() += 1;
            }
            Output::WouldWrite { zone, .. } => {
                *self.would_writes.entry(zone.clone()).or_default() += 1;
            }
            Output::WriteCharger { .. } => self.charger_writes += 1,
            Output::WouldWriteCharger { .. } => self.charger_would_writes += 1,
            Output::PersistChargerWrite(_)
            | Output::PersistChargerShedAllowed(_)
            | Output::PublishStatus(_)
            | Output::PersistHour(_)
            | Output::PersistZoneWrite { .. }
            | Output::PersistZoneBase { .. }
            | Output::PersistOverride { .. }
            | Output::PersistZoneHour { .. }
            | Output::PersistZoneStats { .. }
            | Output::PersistMode(_)
            | Output::PersistShedAllowed { .. }
            | Output::PersistEvent(_) => {}
        }
    }

    /// Renders every metric.
    pub fn render(&self, status: Option<&Status>, mqtt_connected: bool) -> String {
        let mut out = Text::default();
        out.gauge(
            "energybrain_mqtt_connected",
            &[],
            f64::from(u8::from(mqtt_connected)),
        );
        if let Some(s) = status {
            out.gauge("energybrain_hour_used_kwh", &[], s.used.value());
            if let Some(p) = s.projected {
                out.gauge("energybrain_hour_projected_kwh", &[], p.value());
            }
            out.gauge("energybrain_hour_limit_kwh", &[], s.limit.effective.value());
            if let Some(p) = s.power {
                out.gauge("energybrain_meter_power_kw", &[], p.value());
            }
            if let Some(t) = s.meter_last_seen {
                #[allow(clippy::cast_precision_loss)] // epoch seconds fit an f64 exactly
                out.gauge(
                    "energybrain_meter_last_seen_seconds",
                    &[],
                    t.as_second() as f64,
                );
            }
            for (i, day) in s.top_days.iter().enumerate() {
                let rank = (i + 1).to_string();
                out.gauge(
                    "energybrain_month_top_kwh",
                    &[("rank", &rank)],
                    day.kwh.value(),
                );
            }
            out.gauge("energybrain_month_avg_top3_kwh", &[], s.top_average.value());
            #[allow(clippy::cast_precision_loss)] // a handful of tiers
            out.gauge("energybrain_tier", &[], s.tier.index.0 as f64);
            for m in [Mode::Off, Mode::Observe, Mode::Active] {
                out.gauge(
                    "energybrain_mode",
                    &[("mode", m.as_str())],
                    f64::from(u8::from(m == s.mode)),
                );
            }
            for z in &s.zones {
                let id = z.id.as_str();
                for (kind, v) in [
                    ("base", z.base),
                    ("decision", z.decision),
                    ("reported", z.reported),
                ] {
                    if let Some(v) = v {
                        out.gauge(
                            "energybrain_zone_setpoint_celsius",
                            &[("zone", id), ("kind", kind)],
                            v.value(),
                        );
                    }
                }
                out.gauge(
                    "energybrain_zone_shed",
                    &[("zone", id)],
                    f64::from(u8::from(z.shed)),
                );
            }
            if let Some(c) = &s.charger {
                self.charger(&mut out, c);
            }
        }
        for (name, counts) in [
            ("energybrain_zone_writes_total", &self.writes),
            ("energybrain_zone_would_write_total", &self.would_writes),
        ] {
            for (zone, n) in counts {
                #[allow(clippy::cast_precision_loss)] // counts stay far below 2^53
                out.counter(name, &[("zone", zone.as_str())], *n as f64);
            }
        }
        out.0
    }
}

impl Metrics {
    /// The EV charger's metrics (PLAN.md §9). The limit is only present while limited
    /// (0 = paused).
    fn charger(&self, out: &mut Text, c: &ChargerStatus) {
        out.gauge(
            "energybrain_ev_available",
            &[],
            f64::from(u8::from(c.available)),
        );
        if let Some(r) = &c.reading {
            out.gauge("energybrain_ev_power_kw", &[], r.power.value());
            out.gauge("energybrain_ev_offered_amps", &[], r.offered.value());
        }
        if let Some(l) = c.limit {
            out.gauge("energybrain_ev_limit_amps", &[], l.current().value());
        }
        #[allow(clippy::cast_precision_loss)] // counts stay far below 2^53
        {
            out.counter(
                "energybrain_ev_writes_total",
                &[],
                self.charger_writes as f64,
            );
            out.counter(
                "energybrain_ev_would_write_total",
                &[],
                self.charger_would_writes as f64,
            );
        }
    }
}

/// Builds the text format, emitting each `# TYPE` line once.
#[derive(Default)]
struct Text(String, Vec<&'static str>);

impl Text {
    fn gauge(&mut self, name: &'static str, labels: &[(&str, &str)], v: f64) {
        self.sample(name, "gauge", labels, v);
    }

    fn counter(&mut self, name: &'static str, labels: &[(&str, &str)], v: f64) {
        self.sample(name, "counter", labels, v);
    }

    fn sample(&mut self, name: &'static str, kind: &str, labels: &[(&str, &str)], v: f64) {
        if !self.1.contains(&name) {
            self.1.push(name);
            let _ = writeln!(self.0, "# TYPE {name} {kind}");
        }
        let labels: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
        let labels = if labels.is_empty() {
            String::new()
        } else {
            format!("{{{}}}", labels.join(","))
        };
        let _ = writeln!(self.0, "{name}{labels} {v}");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use energybrain_core::charger::{ChargerMode, ChargerReading, ChargerTarget, ChargerWrite};
    use energybrain_core::energy::{Freshness, HourEnergy, Quality};
    use energybrain_core::guard::GuardStatus;
    use energybrain_core::io::{Event, EventKind, ZoneStatus};
    use energybrain_core::peaks::{DayPeak, HourLimit, LimitBasis};
    use energybrain_core::tariff::{Tier, TierIndex};
    use energybrain_core::time::HourStart;
    use energybrain_core::units::Ampere;
    use energybrain_core::units::{Celsius, Kr, Kw, Kwh};
    use jiff::SignedDuration;
    use jiff::Timestamp;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    pub(crate) fn status() -> Status {
        let hour = HourStart::containing(ts("2026-11-15T17:00:00Z"));
        Status {
            at: ts("2026-11-15T17:20:00Z"),
            mode: Mode::Observe,
            guard: GuardStatus::Shedding,
            hour,
            used: Kwh::new(1.5),
            projected: Some(Kwh::new(5.25)),
            power: Some(Kw::new(6.0)),
            freshness: Freshness::Fresh,
            meter_last_seen: Some(ts("2026-11-15T17:19:58Z")),
            limit: HourLimit {
                allowed: Kwh::new(5.0),
                effective: Kwh::new(4.7),
                basis: LimitBasis::TierBound,
            },
            top_days: vec![
                DayPeak {
                    date: jiff::civil::date(2026, 11, 14),
                    hour,
                    kwh: Kwh::new(4.5),
                },
                DayPeak {
                    date: jiff::civil::date(2026, 11, 13),
                    hour,
                    kwh: Kwh::new(4.0),
                },
            ],
            top_average: Kw::new(4.25),
            tier: Tier {
                index: TierIndex(1),
                from: Kw::new(2.0),
                to: Some(Kw::new(5.0)),
                price: Kr::new(233.0),
            },
            zones: vec![ZoneStatus {
                id: ZoneId::new("bathroom").unwrap(),
                name: "Bathroom".to_owned(),
                base: Some(Celsius::new(26.5)),
                reported: Some(Celsius::new(26.5)),
                decision: Some(Celsius::new(22.5)),
                reasons: vec![],
                shed: true,
                shed_allowed: true,
                override_until: None,
                power: Some(Kw::new(1.5)),
                heating_kw: Some(Kw::new(1.5)),
            }],
            charger: None,
        }
    }

    fn bathroom() -> ZoneId {
        ZoneId::new("bathroom").unwrap()
    }

    #[test]
    fn renders_every_metric() {
        let mut m = Metrics::default();
        let write = Output::WriteSetpoint {
            zone: bathroom(),
            value: Celsius::new(22.5),
            reasons: vec![],
        };
        let would = Output::WouldWrite {
            zone: bathroom(),
            value: Celsius::new(22.5),
            reasons: vec![],
        };
        m.record(&write);
        m.record(&would);
        m.record(&would);
        let charger_write = ChargerWrite {
            current: Ampere::new(8.0),
            ttl: SignedDuration::from_mins(10),
        };
        m.record(&Output::WriteCharger {
            write: charger_write,
            reason: String::new(),
        });
        m.record(&Output::WouldWriteCharger {
            write: charger_write,
            reason: String::new(),
        });
        let mut s = status();
        s.charger = Some(ChargerStatus {
            freshness: Freshness::Fresh,
            available: true,
            reading: Some(ChargerReading {
                at: ts("2026-11-15T17:19:40Z"),
                mode: ChargerMode::Charging,
                power: Kw::new(1.84),
                session: Kwh::new(3.0),
                dynamic_current: Some(Ampere::new(8.0)),
                max_current: Some(Ampere::new(16.0)),
                offered: Ampere::new(8.0),
                connected: true,
            }),
            limit: Some(ChargerTarget::Limit(Ampere::new(8.0))),
            shed_allowed: true,
            override_until: None,
        });
        insta::assert_snapshot!(m.render(Some(&s), true));
        // Without a reading or a limit, only availability and the counters.
        s.charger = Some(ChargerStatus {
            freshness: Freshness::NoData,
            available: false,
            reading: None,
            limit: None,
            shed_allowed: true,
            override_until: None,
        });
        let text = m.render(Some(&s), true);
        assert!(text.contains("energybrain_ev_available 0"));
        assert!(!text.contains("energybrain_ev_power_kw") && !text.contains("ev_limit_amps"));
    }

    #[test]
    fn without_a_status_only_connection_and_counters() {
        let mut s = status();
        s.projected = None;
        s.power = None;
        s.meter_last_seen = None;
        s.zones[0].base = None;
        let text = Metrics::default().render(Some(&s), false);
        assert!(
            !text.contains("projected")
                && !text.contains("meter_power")
                && !text.contains("kind=\"base\"")
        );
        assert_eq!(
            Metrics::default().render(None, false),
            "# TYPE energybrain_mqtt_connected gauge\nenergybrain_mqtt_connected 0\n"
        );
    }

    #[test]
    fn other_outputs_are_not_counted() {
        let mut m = Metrics::default();
        let hour = HourStart::containing(ts("2026-11-15T17:00:00Z"));
        let outputs = [
            Output::PublishStatus(Box::new(status())),
            Output::PersistHour(HourEnergy {
                hour,
                kwh: Kwh::ZERO,
                quality: Quality::Measured,
            }),
            Output::PersistZoneWrite {
                zone: bathroom(),
                value: Celsius::new(1.0),
                at: ts("2026-11-15T17:00:00Z"),
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
            Output::PersistChargerWrite(None),
            Output::PersistChargerShedAllowed(true),
            Output::PersistShedAllowed {
                zone: bathroom(),
                allowed: true,
            },
            Output::PersistEvent(Event {
                at: ts("2026-11-15T17:00:00Z"),
                mode: Mode::Off,
                kind: EventKind::OverrideExpired { zone: bathroom() },
            }),
        ];
        for o in &outputs {
            m.record(o);
        }
        assert_eq!(m, Metrics::default());
    }
}

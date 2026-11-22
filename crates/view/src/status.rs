//! The live header: what the engine's latest [`Status`] looks like on the page. The server
//! builds it for every status and streams it to the browser (`/api/stream`).

use energybrain_core::charger::{ChargerMode, ChargerStatus, ChargerTarget};
use energybrain_core::energy::Freshness;
use energybrain_core::guard::GuardStatus;
use energybrain_core::io::{Status, ZoneStatus};
use energybrain_core::tariff::Tier;
use energybrain_core::units::{Ampere, Celsius, Kw, Kwh};
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};

use crate::fmt::{LocalClock, num, qty};

/// A zone counts as heating above this draw (as the engine's power learning does).
const HEATING: f64 = 0.05;

/// How a tile or chip should stand out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    /// Nothing to note.
    Normal,
    /// The guard is acting.
    Warn,
    /// Something is wrong or the limit can't be held.
    Alert,
}

/// One labelled value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tile {
    /// Label.
    pub key: String,
    /// The value, with its unit.
    pub value: String,
    /// A second line.
    pub detail: Option<String>,
    /// Emphasis.
    pub tone: Tone,
}

impl Tile {
    fn new(key: &str, value: impl Into<String>, detail: Option<String>, tone: Tone) -> Self {
        Self {
            key: key.to_owned(),
            value: value.into(),
            detail,
            tone,
        }
    }
}

/// The running hour against its limit, for a progress bar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HourBudget {
    /// "17:00–18:00".
    pub label: String,
    /// Used so far.
    pub used: f64,
    /// Projection for the hour.
    pub projected: Option<f64>,
    /// What the guard acts on.
    pub limit: f64,
    /// "1.20 of 4.70 kWh".
    pub text: String,
    /// "projected 5.20 kWh".
    pub detail: String,
    /// Emphasis: over the limit is an alert, a projection over it a warning.
    pub tone: Tone,
}

/// What a zone is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZoneState {
    /// Not drawing power.
    Idle,
    /// Drawing power.
    Heating,
    /// Shed by the guard (observe mode: would be).
    Shed,
    /// The user overrode it.
    Overridden,
}

/// One zone, for the zone list (and later the house view).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneChip {
    /// Zone id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// State.
    pub state: ZoneState,
    /// The thermostat's setpoint.
    pub setpoint: String,
    /// The user's setpoint when it differs from the thermostat's, "base 26.5 °C".
    pub base: Option<String>,
    /// Its power.
    pub power: String,
    /// Shedding switched off for it.
    pub excluded: bool,
}

/// The header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusBar {
    /// When the status was computed (Unix seconds).
    pub at: i64,
    /// `HH:MM` of `at`, local.
    pub updated: String,
    /// Mode, as configured (`off`, `observe`, `active`).
    pub mode: String,
    /// The running hour.
    pub hour: HourBudget,
    /// Mode, guard, power, EV, month.
    pub tiles: Vec<Tile>,
    /// Per zone, in configuration order.
    pub zones: Vec<ZoneChip>,
}

/// The header for `status`, with local times in `tz`.
pub fn status_bar(status: &Status, tz: &TimeZone) -> StatusBar {
    let hour = status.hour;
    let clock = LocalClock::of(tz, &[hour.start()]);
    let used = status.used.value();
    let projected = status.projected.map(Kwh::value);
    let limit = status.limit.effective.value();
    let hour = HourBudget {
        label: clock.span(hour.start().as_second(), hour.end().as_second()),
        used,
        projected,
        limit,
        text: format!(
            "{} of {}",
            num(Some(used), 2),
            qty(Some(limit), 2, Kwh::UNIT)
        ),
        detail: format!("projected {}", qty(projected, 2, Kwh::UNIT)),
        tone: if used > limit {
            Tone::Alert
        } else if projected.is_some_and(|p| p > limit) {
            Tone::Warn
        } else {
            Tone::Normal
        },
    };
    let mut tiles = vec![
        Tile::new("Mode", status.mode.as_str(), None, Tone::Normal),
        guard_tile(status.guard),
        Tile::new(
            "Power",
            qty(status.power.map(Kw::value), 2, Kw::UNIT),
            match status.freshness {
                Freshness::Fresh => None,
                other => Some(format!("meter {}", freshness(other))),
            },
            if status.freshness == Freshness::Fresh {
                Tone::Normal
            } else {
                Tone::Alert
            },
        ),
    ];
    tiles.extend(status.charger.as_ref().map(ev_tile));
    tiles.push(Tile::new(
        "Month",
        format!(
            "{} top-3 average",
            qty(Some(status.top_average.value()), 2, Kw::UNIT)
        ),
        Some(tier_text(&status.tier)),
        Tone::Normal,
    ));
    StatusBar {
        at: status.at.as_second(),
        updated: LocalClock::of(tz, &[status.at]).hm(status.at.as_second()),
        mode: status.mode.as_str().to_owned(),
        hour,
        tiles,
        zones: status.zones.iter().map(zone_chip).collect(),
    }
}

fn guard_tile(guard: GuardStatus) -> Tile {
    let tone = match guard {
        GuardStatus::Idle | GuardStatus::Disabled => Tone::Normal,
        GuardStatus::Shedding => Tone::Warn,
        GuardStatus::LimitUnreachable | GuardStatus::MeterStale => Tone::Alert,
    };
    Tile::new("Guard", guard.as_str().replace('_', " "), None, tone)
}

fn freshness(f: Freshness) -> &'static str {
    match f {
        Freshness::NoData => "no data",
        Freshness::Fresh => "fresh",
        Freshness::Stale => "stale",
    }
}

/// "2–5 kW · 233 kr/month".
pub fn tier_text(t: &Tier) -> String {
    let to =
        t.to.map_or_else(|| "∞".to_owned(), |to| num(Some(to.value()), 0));
    format!(
        "tier {}–{to} kW · {} kr/month",
        num(Some(t.from.value()), 0),
        num(Some(t.price.value()), 0)
    )
}

/// The charger: what it draws, what it offers, and the guard's limit or the app's override.
fn ev_tile(c: &ChargerStatus) -> Tile {
    let Some(r) = c.reading.filter(|_| c.freshness == Freshness::Fresh) else {
        return Tile::new(
            "EV",
            "–",
            Some(freshness(c.freshness).to_owned()),
            Tone::Alert,
        );
    };
    let limit = if c.override_until.is_some() {
        Some("app limit".to_owned())
    } else {
        c.limit.map(|l| match l {
            ChargerTarget::Pause => "paused".to_owned(),
            ChargerTarget::Limit(a) => format!("limit {}", qty(Some(a.value()), 0, Ampere::UNIT)),
        })
    };
    let detail = [
        Some(charger_mode(r.mode)),
        Some(format!(
            "{} offered",
            qty(Some(r.offered.value()), 0, Ampere::UNIT)
        )),
        limit,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let tone = if c.limit.is_some() {
        Tone::Warn
    } else {
        Tone::Normal
    };
    Tile::new(
        "EV",
        qty(Some(r.power.value()), 2, Kw::UNIT),
        Some(detail),
        tone,
    )
}

fn charger_mode(m: ChargerMode) -> String {
    match m {
        ChargerMode::Offline => "offline".to_owned(),
        ChargerMode::Disconnected => "no car".to_owned(),
        ChargerMode::AwaitingStart => "awaiting start".to_owned(),
        ChargerMode::Charging => "charging".to_owned(),
        ChargerMode::Completed => "completed".to_owned(),
        ChargerMode::Error => "error".to_owned(),
        ChargerMode::ReadyToCharge => "ready to charge".to_owned(),
        ChargerMode::AwaitingAuthentication => "awaiting authentication".to_owned(),
        ChargerMode::Deauthenticating => "deauthenticating".to_owned(),
        ChargerMode::Unknown(code) => format!("mode {code}"),
    }
}

fn zone_chip(z: &ZoneStatus) -> ZoneChip {
    let state = if z.shed {
        ZoneState::Shed
    } else if z.override_until.is_some() {
        ZoneState::Overridden
    } else if z.power.is_some_and(|p| p.value() > HEATING) {
        ZoneState::Heating
    } else {
        ZoneState::Idle
    };
    let celsius = |c: Option<Celsius>| qty(c.map(Celsius::value), 1, Celsius::UNIT);
    ZoneChip {
        id: z.id.to_string(),
        name: z.name.clone(),
        state,
        setpoint: celsius(z.reported),
        base: (z.base != z.reported && z.base.is_some())
            .then(|| format!("base {}", celsius(z.base))),
        power: qty(z.power.map(Kw::value), 2, Kw::UNIT),
        excluded: !z.shed_allowed,
    }
}

#[cfg(test)]
mod tests {
    use energybrain_core::charger::ChargerReading;
    use energybrain_core::ids::ZoneId;
    use energybrain_core::mode::Mode;
    use energybrain_core::peaks::{HourLimit, LimitBasis};
    use energybrain_core::tariff::TierIndex;
    use energybrain_core::time::HourStart;
    use energybrain_core::units::Kr;
    use jiff::Timestamp;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn oslo() -> TimeZone {
        TimeZone::get("Europe/Oslo").unwrap()
    }

    fn zone(id: &str) -> ZoneStatus {
        ZoneStatus {
            id: ZoneId::new(id).unwrap(),
            name: id.to_owned(),
            base: Some(Celsius::new(26.5)),
            reported: Some(Celsius::new(26.5)),
            decision: Some(Celsius::new(26.5)),
            reasons: vec![],
            shed: false,
            shed_allowed: true,
            override_until: None,
            power: Some(Kw::new(0.01)),
            heating_kw: None,
        }
    }

    fn charger(limit: Option<ChargerTarget>) -> ChargerStatus {
        ChargerStatus {
            freshness: Freshness::Fresh,
            available: true,
            reading: Some(ChargerReading {
                at: ts("2026-11-15T17:19:30Z"),
                mode: ChargerMode::Charging,
                power: Kw::new(2.3),
                session: Kwh::new(4.0),
                dynamic_current: None,
                max_current: Some(Ampere::new(16.0)),
                offered: Ampere::new(16.0),
                connected: true,
            }),
            limit,
            shed_allowed: true,
            override_until: None,
        }
    }

    pub(crate) fn status() -> Status {
        let shed = ZoneStatus {
            shed: true,
            reported: Some(Celsius::new(22.5)),
            decision: Some(Celsius::new(22.5)),
            power: Some(Kw::new(1.4)),
            ..zone("bathroom")
        };
        let heating = ZoneStatus {
            power: Some(Kw::new(0.6)),
            shed_allowed: false,
            ..zone("laundry")
        };
        let overridden = ZoneStatus {
            override_until: Some(ts("2026-11-15T19:00:00Z")),
            ..zone("hallway")
        };
        Status {
            at: ts("2026-11-15T17:20:00Z"),
            mode: Mode::Observe,
            guard: GuardStatus::Shedding,
            hour: HourStart::containing(ts("2026-11-15T17:20:00Z")),
            used: Kwh::new(1.2),
            projected: Some(Kwh::new(5.2)),
            power: Some(Kw::new(6.0)),
            freshness: Freshness::Fresh,
            meter_last_seen: Some(ts("2026-11-15T17:19:58Z")),
            limit: HourLimit {
                allowed: Kwh::new(5.0),
                effective: Kwh::new(4.7),
                basis: LimitBasis::TierBound,
            },
            top_days: vec![],
            top_average: Kw::new(3.4),
            tier: Tier {
                index: TierIndex(1),
                from: Kw::new(2.0),
                to: Some(Kw::new(5.0)),
                price: Kr::new(233.0),
            },
            zones: vec![shed, heating, overridden, zone("basement")],
            charger: Some(charger(Some(ChargerTarget::Limit(Ampere::new(10.0))))),
        }
    }

    #[test]
    fn status_bar_json() {
        let bar = status_bar(&status(), &oslo());
        insta::assert_snapshot!(serde_json::to_string_pretty(&bar).unwrap());
        let back: StatusBar = serde_json::from_str(&serde_json::to_string(&bar).unwrap()).unwrap();
        assert_eq!(back, bar);
    }

    #[test]
    fn the_hour_tone_follows_used_and_projected() {
        let mut s = status();
        let tone = |s: &Status| status_bar(s, &oslo()).hour.tone;
        assert_eq!(tone(&s), Tone::Warn);
        s.projected = None;
        assert_eq!(tone(&s), Tone::Normal);
        s.used = Kwh::new(4.8);
        assert_eq!(tone(&s), Tone::Alert);
    }

    #[test]
    fn guard_and_meter_tones() {
        let tiles = |s: &Status| status_bar(s, &oslo()).tiles;
        let mut s = status();
        for (guard, tone) in [
            (GuardStatus::Idle, Tone::Normal),
            (GuardStatus::Disabled, Tone::Normal),
            (GuardStatus::Shedding, Tone::Warn),
            (GuardStatus::LimitUnreachable, Tone::Alert),
            (GuardStatus::MeterStale, Tone::Alert),
        ] {
            s.guard = guard;
            assert_eq!(tiles(&s)[1].tone, tone, "{guard:?}");
        }
        s.freshness = Freshness::Stale;
        s.power = None;
        let power = &tiles(&s)[2];
        assert_eq!(
            (power.value.as_str(), power.detail.as_deref(), power.tone),
            ("–", Some("meter stale"), Tone::Alert)
        );
        s.freshness = Freshness::NoData;
        assert_eq!(tiles(&s)[2].detail.as_deref(), Some("meter no data"));
        // Without a charger there is no EV tile.
        s.charger = None;
        assert_eq!(
            tiles(&s).iter().map(|t| t.key.as_str()).collect::<Vec<_>>(),
            ["Mode", "Guard", "Power", "Month"]
        );
    }

    #[test]
    fn ev_tile_variants() {
        let detail = |c: &ChargerStatus| {
            let t = ev_tile(c);
            (t.value, t.detail.unwrap(), t.tone)
        };
        assert_eq!(
            detail(&charger(Some(ChargerTarget::Pause))),
            (
                "2.30 kW".to_owned(),
                "charging · 16 A offered · paused".to_owned(),
                Tone::Warn
            )
        );
        assert_eq!(
            detail(&charger(None)),
            (
                "2.30 kW".to_owned(),
                "charging · 16 A offered".to_owned(),
                Tone::Normal
            )
        );
        let mut c = charger(Some(ChargerTarget::Pause));
        c.override_until = Some(ts("2026-11-15T19:00:00Z"));
        assert!(detail(&c).1.ends_with("app limit"));
        c.freshness = Freshness::Stale;
        assert_eq!(
            detail(&c),
            ("–".to_owned(), "stale".to_owned(), Tone::Alert)
        );
        c.freshness = Freshness::Fresh;
        c.reading = None;
        assert_eq!(detail(&c).0, "–");
    }

    #[test]
    fn charger_modes_read_as_words() {
        let modes = [
            ChargerMode::Offline,
            ChargerMode::Disconnected,
            ChargerMode::AwaitingStart,
            ChargerMode::Charging,
            ChargerMode::Completed,
            ChargerMode::Error,
            ChargerMode::ReadyToCharge,
            ChargerMode::AwaitingAuthentication,
            ChargerMode::Deauthenticating,
            ChargerMode::Unknown(42),
        ];
        assert_eq!(
            modes.map(charger_mode),
            [
                "offline",
                "no car",
                "awaiting start",
                "charging",
                "completed",
                "error",
                "ready to charge",
                "awaiting authentication",
                "deauthenticating",
                "mode 42"
            ]
        );
    }

    #[test]
    fn the_top_tier_is_open_ended() {
        let t = Tier {
            index: TierIndex(3),
            from: Kw::new(10.0),
            to: None,
            price: Kr::new(585.0),
        };
        assert_eq!(tier_text(&t), "tier 10–∞ kW · 585 kr/month");
    }
}

//! The day page: a [`DayView`] turned into what the browser draws, with every label and
//! description already written.

use energybrain_core::energy::Quality;
use energybrain_core::guard::GuardStatus;
use energybrain_core::units::Kwh;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fmt::{LocalClock, num, qty};
use crate::history::{DayView, Series, Site};

/// A guard state worth shading in the hour-budget chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    /// Shedding.
    Shedding,
    /// Shedding everything is not enough.
    LimitUnreachable,
    /// No fresh meter data.
    MeterStale,
}

/// One local hour of the day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HourBar {
    /// Start (Unix seconds).
    pub start: i64,
    /// End (Unix seconds).
    pub end: i64,
    /// "17:00".
    pub label: String,
    /// "17:00–18:00".
    pub title: String,
    /// "1.20 kWh of 4.70 kWh (estimated)": the hour at a glance.
    pub summary: String,
    /// Whole-house energy.
    pub energy: Option<f64>,
    /// Estimated at the rollover, not yet measured.
    pub estimated: bool,
    /// Allowed energy, before the safety margin.
    pub allowed: Option<f64>,
    /// What the guard acted on.
    pub limit: Option<f64>,
    /// The energy went over the limit.
    pub over: bool,
}

/// The sample log, one entry per minute (gaps are all `None`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Samples {
    /// Sample times (Unix seconds).
    pub at: Vec<i64>,
    /// House power, kW.
    pub power: Vec<Option<f64>>,
    /// The floors' power together, kW.
    pub floors: Vec<Option<f64>>,
    /// Energy used so far in the hour, kWh.
    pub used: Vec<Option<f64>>,
    /// Projection for the hour, kWh.
    pub projected: Vec<Option<f64>>,
    /// What the guard acts on, kWh.
    pub limit: Vec<Option<f64>>,
    /// Guard states to shade.
    pub band: Vec<Option<Band>>,
}

/// The EV charger's samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvSamples {
    /// The charger's power, kW.
    pub power: Vec<Option<f64>>,
    /// Floors + charger, kW (for stacking).
    pub stacked: Vec<Option<f64>>,
    /// Offered current, A.
    pub offered: Vec<Option<f64>>,
    /// The user's charger limit, A.
    pub max: Vec<Option<f64>>,
    /// The guard's limit, A (0 is a pause).
    pub limit: Vec<Option<f64>>,
    /// The app's temporary limit overrode the guard.
    pub overridden: Vec<bool>,
}

/// One zone's samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneSamples {
    /// Display name.
    pub name: String,
    /// What the thermostat reported, °C.
    pub reported: Vec<Option<f64>>,
    /// What the engine wanted, °C.
    pub decision: Vec<Option<f64>>,
    /// The user's setpoint, °C.
    pub base: Vec<Option<f64>>,
    /// Its power, kW.
    pub power: Vec<Option<f64>>,
    /// Shed by the guard.
    pub shed: Vec<bool>,
    /// Drawing power (above the same threshold the status bar uses).
    pub heating: Vec<bool>,
}

/// One stored event, described.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRow {
    /// When (Unix seconds).
    pub at: i64,
    /// "17:05".
    pub time: String,
    /// "Would shed".
    pub what: String,
    /// Zone, "EV", or "–".
    pub zone: String,
    /// The numbers behind it.
    pub detail: String,
}

/// The events of one hour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventGroup {
    /// Index into [`DayPage::hours`].
    pub hour: usize,
    /// "17:00–18:00".
    pub title: String,
    /// "4.50 / 4.70 kWh · 2 events".
    pub summary: String,
    /// The hour went over its limit.
    pub over: bool,
    /// The events, newest first.
    pub rows: Vec<EventRow>,
}

/// The day page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayPage {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// "Sunday 15 November 2026".
    pub title: String,
    /// The day before.
    pub prev: String,
    /// The day after.
    pub next: String,
    /// Whether this is today (it keeps filling in).
    pub is_today: bool,
    /// IANA time zone the times are in.
    pub time_zone: String,
    /// First instant (Unix seconds).
    pub from: i64,
    /// First instant of the next day.
    pub to: i64,
    /// Local time for any instant in the day.
    pub clock: LocalClock,
    /// One per local hour (23, 24 or 25).
    pub hours: Vec<HourBar>,
    /// The sample log.
    pub samples: Samples,
    /// The charger, on days it was sampled.
    pub ev: Option<EvSamples>,
    /// Per zone, in configuration order.
    pub zones: Vec<ZoneSamples>,
    /// Events by hour, newest hour first.
    pub events: Vec<EventGroup>,
}

fn values(s: &Series) -> Vec<Option<f64>> {
    s.values.clone()
}

fn flags(v: &[Option<bool>]) -> Vec<bool> {
    v.iter().map(|b| b.unwrap_or(false)).collect()
}

/// One bar per local hour.
fn hour_bars(clock: &LocalClock, h: &crate::history::HourColumns, to: i64) -> Vec<HourBar> {
    h.start
        .iter()
        .enumerate()
        .map(|(i, start)| {
            let start = start.as_second();
            let energy = h.energy.values[i];
            let limit = h.limit.values[i];
            let end = h.start.get(i + 1).map_or(to, |t| t.as_second());
            let estimated = h.quality[i] == Some(Quality::Estimated);
            HourBar {
                start,
                end,
                label: clock.hm(start),
                title: clock.span(start, end),
                summary: hour_summary(energy, limit, estimated),
                energy,
                estimated,
                allowed: h.allowed.values[i],
                limit,
                over: energy.zip(limit).is_some_and(|(e, l)| e > l),
            }
        })
        .collect()
}

/// The page for `view`.
pub fn day_page(site: &Site, view: &DayView) -> DayPage {
    let clock = LocalClock::of(site.calendar.time_zone(), &view.hours.start);
    let to = view.to.as_second();
    let hours = hour_bars(&clock, &view.hours, to);
    let s = &view.samples;
    let samples = Samples {
        at: s.at.iter().map(|t| t.as_second()).collect(),
        power: values(&s.power),
        floors: values(&s.floors),
        used: values(&s.used),
        projected: values(&s.projected),
        limit: values(&s.limit),
        band: s
            .guard
            .iter()
            .map(|g| match g {
                Some(GuardStatus::Shedding) => Some(Band::Shedding),
                Some(GuardStatus::LimitUnreachable) => Some(Band::LimitUnreachable),
                Some(GuardStatus::MeterStale) => Some(Band::MeterStale),
                Some(GuardStatus::Idle | GuardStatus::Disabled) | None => None,
            })
            .collect(),
    };
    let ev = s.ev.as_ref().map(|e| EvSamples {
        power: values(&e.power),
        stacked: values(&e.stacked),
        offered: values(&e.offered),
        max: values(&e.max),
        limit: values(&e.limit),
        overridden: flags(&e.overridden),
    });
    let zones = s
        .zones
        .iter()
        .map(|z| ZoneSamples {
            name: z.name.clone(),
            reported: values(&z.reported),
            decision: values(&z.decision),
            base: values(&z.base),
            power: values(&z.power),
            shed: flags(&z.shed),
            heating: z
                .power
                .values
                .iter()
                .map(|p| p.is_some_and(crate::status::heating))
                .collect(),
        })
        .collect();
    let events = view
        .event_hours
        .iter()
        .map(|g| {
            let n = g.events.len();
            let bar = &hours[g.hour];
            EventGroup {
                hour: g.hour,
                title: clock.span(g.start.as_second(), g.end.as_second()),
                summary: format!(
                    "{} / {} · {n} {}",
                    num(bar.energy, 2),
                    qty(bar.limit, 2, Kwh::UNIT),
                    if n == 1 { "event" } else { "events" }
                ),
                over: g.over_limit,
                rows: g.events.iter().map(|e| event_row(&clock, e)).collect(),
            }
        })
        .collect();
    DayPage {
        date: view.date.to_string(),
        title: crate::fmt::long_date(view.date),
        // `day_range` already needed the next day, so only the day before can be missing.
        prev: view.date.yesterday().unwrap_or(view.date).to_string(),
        next: view.date.tomorrow().unwrap_or(view.date).to_string(),
        is_today: view.date == view.today,
        time_zone: view.time_zone.clone(),
        from: view.from.as_second(),
        to,
        clock,
        hours,
        samples,
        ev,
        zones,
        events,
    }
}

/// "1.20 kWh of 4.70 kWh (estimated)", "no data".
fn hour_summary(energy: Option<f64>, limit: Option<f64>, estimated: bool) -> String {
    let Some(e) = energy else {
        return "no data".to_owned();
    };
    let of = limit.map(|l| format!(" of {}", qty(Some(l), 2, Kwh::UNIT)));
    format!(
        "{}{}{}",
        qty(Some(e), 2, Kwh::UNIT),
        of.unwrap_or_default(),
        if estimated { " (estimated)" } else { "" }
    )
}

/// `{"value": 4.7, "unit": "kWh"}` as text.
fn quantity(v: &Value, digits: usize) -> String {
    match (v["value"].as_f64(), v["unit"].as_str()) {
        (Some(x), Some(unit)) => qty(Some(x), digits, unit),
        _ => crate::fmt::DASH.to_owned(),
    }
}

fn release_reason(v: &Value) -> String {
    let reason = v.as_str().unwrap_or_default();
    match reason {
        "below_limit" => "projection back below the limit",
        "meter_stale" => "meter stale",
        "not_sheddable" => "no longer sheddable (switched off or overridden)",
        "guard_off" => "guard turned off",
        "car_disconnected" => "car unplugged",
        "charger_unavailable" => "charger reading stale or offline",
        other => other,
    }
    .to_owned()
}

fn time_of(clock: &LocalClock, v: &Value) -> String {
    v.as_str()
        .and_then(|s| s.parse::<Timestamp>().ok())
        .map_or_else(|| crate::fmt::DASH.to_owned(), |t| clock.hm(t.as_second()))
}

fn dash() -> String {
    crate::fmt::DASH.to_owned()
}

/// `(what, zone, detail)` of a guard decision.
fn guard_event(g: &Value, would: bool) -> (String, String, String) {
    let w = |would_text: &str, did: &str| if would { would_text } else { did }.to_owned();
    let budget = |extra: &str, key: &str| {
        format!(
            "projected {}, limit {}, {extra} {}",
            quantity(&g["projected"], 2),
            quantity(&g["limit"], 2),
            quantity(&g[key], 2)
        )
    };
    match g["type"].as_str() {
        Some("shed") => (
            w("Would shed", "Shed"),
            name(&g["zone"]),
            budget("load", "load"),
        ),
        Some("release") => (
            w("Would release", "Released"),
            name(&g["zone"]),
            release_reason(&g["reason"]),
        ),
        Some("charger_limit") => {
            let what = if g["current"]["value"].as_f64() == Some(0.0) {
                w("Would pause EV", "Paused EV")
            } else {
                format!(
                    "{} {}",
                    w("Would limit EV to", "Limited EV to"),
                    quantity(&g["current"], 0)
                )
            };
            (what, "EV".to_owned(), budget("saves", "saving"))
        }
        Some("charger_release") => (
            w("Would release EV", "Released EV"),
            "EV".to_owned(),
            release_reason(&g["reason"]),
        ),
        Some("limit_unreachable") => (
            "Limit unreachable".to_owned(),
            dash(),
            budget("shedding saves", "available"),
        ),
        other => (other.unwrap_or("guard").to_owned(), dash(), g.to_string()),
    }
}

fn name(v: &Value) -> String {
    v.as_str().map_or_else(dash, str::to_owned)
}

/// Describes a stored event (its JSON, as the engine wrote it). Unknown kinds show their JSON.
pub fn event_row(clock: &LocalClock, e: &Value) -> EventRow {
    let at = e["at"]
        .as_str()
        .and_then(|s| s.parse::<Timestamp>().ok())
        .map_or(0, Timestamp::as_second);
    let k = &e["kind"];
    let (what, zone, detail) = match k["type"].as_str() {
        Some("guard") => guard_event(&k["event"], e["mode"].as_str() != Some("active")),
        Some("override") => (
            "Override".to_owned(),
            name(&k["zone"]),
            format!(
                "{} until {}",
                quantity(&k["setpoint"], 1),
                time_of(clock, &k["until"])
            ),
        ),
        Some("override_expired") => ("Override ended".to_owned(), name(&k["zone"]), String::new()),
        Some("charger_override") => (
            "Temporary limit".to_owned(),
            "EV".to_owned(),
            format!(
                "{} from the app, guard leaves the charger alone until {}",
                quantity(&k["current"], 0),
                time_of(clock, &k["until"])
            ),
        ),
        Some("mode_changed") => (
            "Mode changed".to_owned(),
            dash(),
            format!(
                "{} → {}",
                k["from"].as_str().unwrap_or_default(),
                k["to"].as_str().unwrap_or_default()
            ),
        ),
        kind => (kind.unwrap_or("event").to_owned(), dash(), k.to_string()),
    };
    EventRow {
        at,
        time: clock.hm(at),
        what,
        zone,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use energybrain_core::guard::{GuardEvent, ReleaseReason};
    use energybrain_core::ids::ZoneId;
    use energybrain_core::io::ChargerSample;
    use energybrain_core::io::{Event, EventKind};
    use energybrain_core::mode::Mode;
    use energybrain_core::units::{Ampere, Celsius, Kw};
    use jiff::civil::date;

    use super::*;
    use crate::history::fixtures::{day_history, site, ts};
    use crate::history::{day_range, day_view};

    fn page_of(d: jiff::civil::Date, history: crate::history::History) -> DayPage {
        let site = site();
        let range = day_range(&site.calendar, d).unwrap();
        day_page(
            &site,
            &day_view(&site, d, range, date(2026, 11, 15), history),
        )
    }

    #[test]
    fn day_page_json() {
        let page = page_of(date(2026, 11, 15), day_history());
        insta::assert_snapshot!(serde_json::to_string_pretty(&page).unwrap());
        let back: DayPage = serde_json::from_str(&serde_json::to_string(&page).unwrap()).unwrap();
        assert_eq!(back, page);
    }

    #[test]
    fn a_zone_heats_while_it_draws_more_than_the_threshold() {
        let mut history = day_history();
        history.samples[1].zones[0].power = Some(Kw::new(0.05));
        history.samples[2].zones[0].power = None;
        let page = page_of(date(2026, 11, 15), history);
        let zone = &page.zones[0];
        assert_eq!(zone.name, "Bathroom");
        // The fourth point is the gap while the service was down.
        assert_eq!(zone.heating, [true, false, false, false, true]);
    }

    #[test]
    fn fall_back_has_25_hours_and_the_repeated_hour_twice() {
        let page = page_of(date(2026, 10, 25), crate::history::History::default());
        assert_eq!(page.hours.len(), 25);
        let labels: Vec<&str> = page.hours[..4].iter().map(|h| h.label.as_str()).collect();
        assert_eq!(labels, ["00:00", "01:00", "02:00", "02:00"]);
        assert_eq!(page.hours[24].end, page.to);
        assert!(!page.is_today);
        assert_eq!(
            (page.prev.as_str(), page.next.as_str()),
            ("2026-10-24", "2026-10-26")
        );
    }

    #[test]
    fn guard_states_become_bands() {
        let mut h = day_history();
        h.samples[0].guard = GuardStatus::LimitUnreachable;
        h.samples[1].guard = GuardStatus::MeterStale;
        h.samples[2].guard = GuardStatus::Disabled;
        let page = page_of(date(2026, 11, 15), h);
        assert_eq!(
            page.samples.band,
            [
                Some(Band::LimitUnreachable),
                Some(Band::MeterStale),
                None,
                None,
                None
            ]
        );
    }

    #[test]
    fn the_charger_gets_its_samples() {
        let mut h = day_history();
        h.samples[0].charger = Some(ChargerSample {
            power: Some(Kw::new(2.3)),
            offered: Some(Ampere::new(10.0)),
            max: Some(Ampere::new(16.0)),
            limit: Some(Ampere::new(10.0)),
            overridden: true,
        });
        let ev = page_of(date(2026, 11, 15), h).ev.unwrap();
        assert_eq!(ev.offered[0], Some(10.0));
        assert_eq!(ev.overridden, [true, false, false, false, false]);
    }

    fn row(mode: Mode, kind: EventKind) -> EventRow {
        let e = serde_json::to_value(Event {
            at: ts("2026-11-15T17:05:00Z"),
            mode,
            kind,
        })
        .unwrap();
        event_row(&page_of(date(2026, 11, 15), day_history()).clock, &e)
    }

    fn text(r: &EventRow) -> String {
        format!("{} | {} | {} | {}", r.time, r.what, r.zone, r.detail)
    }

    #[test]
    fn events_are_described() {
        let zone = || ZoneId::new("bathroom").unwrap();
        let guard = |event| EventKind::Guard { event };
        let release = |reason| {
            guard(GuardEvent::Release {
                zone: zone(),
                reason,
            })
        };
        let charger = |a: f64| {
            guard(GuardEvent::ChargerLimit {
                current: Ampere::new(a),
                saving: Kw::new(0.9),
                projected: Kwh::new(5.1),
                limit: Kwh::new(4.7),
            })
        };
        let rows = [
            row(
                Mode::Observe,
                guard(GuardEvent::Shed {
                    zone: zone(),
                    load: Kw::new(1.4),
                    projected: Kwh::new(5.2),
                    limit: Kwh::new(4.7),
                }),
            ),
            row(Mode::Active, release(ReleaseReason::BelowLimit)),
            row(Mode::Observe, release(ReleaseReason::MeterStale)),
            row(Mode::Active, release(ReleaseReason::NotSheddable)),
            row(Mode::Active, release(ReleaseReason::GuardOff)),
            row(Mode::Observe, charger(10.0)),
            row(Mode::Active, charger(10.0)),
            row(Mode::Observe, charger(0.0)),
            row(Mode::Active, charger(0.0)),
            row(
                Mode::Observe,
                guard(GuardEvent::ChargerRelease {
                    reason: ReleaseReason::CarDisconnected,
                }),
            ),
            row(
                Mode::Active,
                guard(GuardEvent::ChargerRelease {
                    reason: ReleaseReason::ChargerUnavailable,
                }),
            ),
            row(
                Mode::Active,
                guard(GuardEvent::LimitUnreachable {
                    projected: Kwh::new(7.0),
                    limit: Kwh::new(4.7),
                    available: Kwh::new(1.0),
                }),
            ),
            row(
                Mode::Active,
                EventKind::Override {
                    zone: zone(),
                    value: Celsius::new(24.0),
                    until: ts("2026-11-15T19:05:00Z"),
                },
            ),
            row(Mode::Active, EventKind::OverrideExpired { zone: zone() }),
            row(
                Mode::Active,
                EventKind::ChargerOverride {
                    current: Ampere::new(8.0),
                    until: ts("2026-11-15T19:05:00Z"),
                },
            ),
            row(
                Mode::Active,
                EventKind::ModeChanged {
                    from: Mode::Observe,
                    to: Mode::Active,
                },
            ),
        ];
        insta::assert_snapshot!(rows.iter().map(text).collect::<Vec<_>>().join("\n"));
    }

    #[test]
    fn unknown_and_malformed_events_still_show() {
        let clock = LocalClock::default();
        let r = event_row(
            &clock,
            &serde_json::json!({ "kind": { "type": "something_new", "x": 1 } }),
        );
        assert_eq!(
            text(&r),
            r#"00:00 | something_new | – | {"type":"something_new","x":1}"#
        );
        let r = event_row(&clock, &serde_json::json!({}));
        assert_eq!(text(&r), "00:00 | event | – | null");
        let r = event_row(
            &clock,
            &serde_json::json!({ "kind": { "type": "override", "setpoint": 3, "until": 5 } }),
        );
        assert_eq!(text(&r), "00:00 | Override | – | – until –");
        let r = event_row(
            &clock,
            &serde_json::json!({ "kind": { "type": "guard", "event": { "type": "release", "reason": "new_reason" } } }),
        );
        assert_eq!(r.detail, "new_reason");
        let r = event_row(
            &clock,
            &serde_json::json!({ "kind": { "type": "guard", "event": { "type": "new_decision" } } }),
        );
        assert_eq!(
            text(&r),
            r#"00:00 | new_decision | – | {"type":"new_decision"}"#
        );
        let r = event_row(&clock, &serde_json::json!({ "kind": { "type": "guard" } }));
        assert_eq!(text(&r), "00:00 | guard | – | null");
    }
}

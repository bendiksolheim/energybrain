//! The peak guard: decides which zones to shed so the running hour stays under the limit.
//!
//! Each evaluation:
//! 1. Zones that may no longer be shed (switched off in HomeKit, user override, guard off)
//!    are released immediately.
//! 2. With a stale meter nothing new is shed and shed zones are released one per stagger.
//! 3. The projection is reduced by a **credit** for shed zones whose reduction the meter
//!    cannot show: a zone still drawing power (always the case in observe mode, where nothing
//!    is really written) is credited its whole saving since it was shed, `load × (hour end −
//!    shed time)`; a zone that really switched off is credited `load × remaining` until
//!    `settle` has passed (the meter's moving average needs time to fall). Without this the
//!    guard would keep shedding more zones.
//! 4. Above the limit, eligible zones are shed in priority order (**lower number first**)
//!    until their expected reduction `load × remaining_fraction` covers the excess. If even
//!    all of them cannot, they are all shed and `LimitUnreachable` is reported.
//! 5. Below the limit, the most recently shed zone is released once it has been shed for
//!    `min_shed_duration`, the projection *with it back on* stays below
//!    `limit − release_margin`, and `restore_stagger` has passed since the last release.

use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp};

use crate::energy::{Freshness, MeterSnapshot};
use crate::ids::ZoneId;
use crate::peaks::HourLimit;
use crate::time::HourStart;
use crate::units::{Kw, Kwh};
use crate::zone::{Zone, ZoneSettings};

/// Guard tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuardConfig {
    /// Hysteresis: release only below `limit − release_margin`.
    pub release_margin: Kwh,
    /// A zone stays shed at least this long (unless it stops being sheddable).
    pub min_shed_duration: SignedDuration,
    /// At most one staggered release per this interval.
    pub restore_stagger: SignedDuration,
    /// How long a fresh shed is credited before the meter is trusted to show it.
    pub settle: SignedDuration,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            release_margin: Kwh::new(0.3),
            min_shed_duration: SignedDuration::from_mins(5),
            restore_stagger: SignedDuration::from_mins(2),
            settle: SignedDuration::from_secs(90),
        }
    }
}

/// A zone the guard has shed.
#[derive(Debug, Clone, PartialEq)]
pub struct Shed {
    /// The zone.
    pub zone: ZoneId,
    /// When it was shed.
    pub since: Timestamp,
    /// Its load estimate when shed; the expected reduction and comeback load.
    pub load: Kw,
    /// Why it was shed.
    pub reason: String,
}

/// The guard's overall state, for status and HomeKit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardStatus {
    /// Nothing shed.
    #[default]
    Idle,
    /// Some zones shed, limit reachable.
    Shedding,
    /// Shedding every eligible zone is not enough this hour.
    LimitUnreachable,
    /// No fresh meter data; not shedding.
    MeterStale,
    /// Mode is off.
    Disabled,
}

/// Why a zone was released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    /// The projection is comfortably below the limit.
    BelowLimit,
    /// No fresh meter data.
    MeterStale,
    /// Shedding was switched off for the zone, or the user overrode it.
    NotSheddable,
    /// The guard was turned off.
    GuardOff,
}

/// Something the guard did, for the event log.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuardEvent {
    /// A zone was shed.
    Shed {
        /// The zone.
        zone: ZoneId,
        /// Its load estimate.
        load: Kw,
        /// Projection (after credits) that triggered it.
        projected: Kwh,
        /// Effective limit.
        limit: Kwh,
    },
    /// A zone was released.
    Release {
        /// The zone.
        zone: ZoneId,
        /// Why.
        reason: ReleaseReason,
    },
    /// Even shedding every eligible zone cannot meet the limit.
    LimitUnreachable {
        /// Projection (after credits).
        projected: Kwh,
        /// Effective limit.
        limit: Kwh,
        /// What shedding everything eligible would save.
        available: Kwh,
    },
}

/// What the guard looks at.
#[derive(Debug, Clone, Copy)]
pub struct GuardInput<'a> {
    /// The current instant.
    pub now: Timestamp,
    /// The running hour.
    pub meter: &'a MeterSnapshot,
    /// The running hour's limit.
    pub limit: &'a HourLimit,
    /// All zones.
    pub zones: &'a BTreeMap<ZoneId, Zone>,
    /// Zone settings (heating threshold).
    pub settings: &'a ZoneSettings,
    /// False when the mode is `Off`.
    pub enabled: bool,
}

/// The guard's mutable state. Owned by the engine.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GuardState {
    /// Shed zones, in the order they were shed.
    pub shed: Vec<Shed>,
    /// When the last staggered release happened.
    pub last_release: Option<Timestamp>,
    /// Overall status after the last evaluation.
    pub status: GuardStatus,
    /// Savings of zones released while still drawing (observe mode), for the hour they
    /// belong to. The meter never saw them, so they stay credited until the hour ends.
    pub banked: Option<(HourStart, Kwh)>,
}

impl GuardState {
    /// Whether `zone` is currently shed.
    pub fn is_shed(&self, zone: &ZoneId) -> bool {
        self.shed.iter().any(|s| &s.zone == zone)
    }

    /// Re-evaluates the guard and returns what changed.
    pub fn update(&mut self, cfg: &GuardConfig, input: &GuardInput<'_>) -> Vec<GuardEvent> {
        let now = input.now;
        let hour = input.meter.hour;
        let mut events = Vec::new();
        if self.banked.is_some_and(|(h, _)| h != hour) {
            self.banked = None;
        }

        let reason = if input.enabled {
            ReleaseReason::NotSheddable
        } else {
            ReleaseReason::GuardOff
        };
        let (keep, dropped): (Vec<Shed>, Vec<Shed>) =
            std::mem::take(&mut self.shed).into_iter().partition(|s| {
                input.enabled && input.zones.get(&s.zone).is_some_and(|z| z.sheddable(now))
            });
        self.shed = keep;
        for s in dropped {
            self.bank(input, &s);
            events.push(GuardEvent::Release {
                zone: s.zone,
                reason,
            });
        }
        if !input.enabled {
            self.status = GuardStatus::Disabled;
            return events;
        }

        let (Freshness::Fresh, Some(raw)) = (input.meter.freshness, input.meter.projected) else {
            self.status = GuardStatus::MeterStale;
            self.release_staggered(cfg, input, ReleaseReason::MeterStale, &mut events);
            return events;
        };

        let remaining = hour.remaining_fraction(now);
        let credit: Kwh = self
            .shed
            .iter()
            .map(|s| {
                let drawing = input
                    .zones
                    .get(&s.zone)
                    .is_some_and(|z| z.is_drawing(input.settings));
                if drawing {
                    // Nothing of the shed shows in the meter (observe mode, or a thermostat
                    // that did not react): credit the whole saving since the shed.
                    s.load * hour.end().duration_since(s.since.max(hour.start()))
                } else if now.duration_since(s.since) < cfg.settle {
                    // Really off, but the moving average has not caught up yet.
                    s.load.for_one_hour() * remaining
                } else {
                    Kwh::ZERO
                }
            })
            .sum();
        let banked = self.banked.map_or(Kwh::ZERO, |(_, k)| k);
        let projected = raw - credit - banked;
        let limit = input.limit.effective;

        if projected > limit {
            self.shed_more(input, projected, limit, remaining, &mut events);
        } else if let Some(last) = self.shed.last() {
            let comeback = last.load.for_one_hour() * remaining;
            if now.duration_since(last.since) >= cfg.min_shed_duration
                && projected + comeback < limit - cfg.release_margin
            {
                self.release_staggered(cfg, input, ReleaseReason::BelowLimit, &mut events);
            }
            self.status = self.settled_status();
        } else {
            self.status = GuardStatus::Idle;
        }
        events
    }

    fn shed_more(
        &mut self,
        input: &GuardInput<'_>,
        projected: Kwh,
        limit: Kwh,
        remaining: f64,
        events: &mut Vec<GuardEvent>,
    ) {
        let now = input.now;
        let mut candidates: Vec<&Zone> = input
            .zones
            .values()
            .filter(|z| z.sheddable(now) && !self.is_shed(z.id()) && z.is_drawing(input.settings))
            .collect();
        candidates.sort_by_key(|z| (z.config.priority, z.id().clone()));
        let reduction = |z: &Zone| z.load().for_one_hour() * remaining;
        let available: Kwh = candidates.iter().map(|z| reduction(z)).sum();
        let mut excess = projected - limit;
        let unreachable = available < excess;
        for z in candidates {
            if excess <= Kwh::ZERO {
                break;
            }
            excess -= reduction(z);
            self.shed.push(Shed {
                zone: z.id().clone(),
                since: now,
                load: z.load(),
                reason: format!("projected {projected:.2} > limit {limit:.2}"),
            });
            events.push(GuardEvent::Shed {
                zone: z.id().clone(),
                load: z.load(),
                projected,
                limit,
            });
        }
        if unreachable {
            if self.status != GuardStatus::LimitUnreachable {
                events.push(GuardEvent::LimitUnreachable {
                    projected,
                    limit,
                    available,
                });
            }
            self.status = GuardStatus::LimitUnreachable;
        } else {
            self.status = self.settled_status();
        }
    }

    fn release_staggered(
        &mut self,
        cfg: &GuardConfig,
        input: &GuardInput<'_>,
        reason: ReleaseReason,
        events: &mut Vec<GuardEvent>,
    ) {
        let now = input.now;
        if self
            .last_release
            .is_some_and(|t| now.duration_since(t) < cfg.restore_stagger)
        {
            return;
        }
        if let Some(s) = self.shed.pop() {
            self.last_release = Some(now);
            self.bank(input, &s);
            events.push(GuardEvent::Release {
                zone: s.zone,
                reason,
            });
        }
    }

    /// Banks the saving of a zone released while still drawing.
    fn bank(&mut self, input: &GuardInput<'_>, s: &Shed) {
        let drawing = input
            .zones
            .get(&s.zone)
            .is_some_and(|z| z.is_drawing(input.settings));
        if drawing {
            let hour = input.meter.hour;
            let saved = s.load * input.now.duration_since(s.since.max(hour.start()));
            let total = self.banked.map_or(Kwh::ZERO, |(_, k)| k) + saved.max(Kwh::ZERO);
            self.banked = Some((hour, total));
        }
    }

    fn settled_status(&self) -> GuardStatus {
        if self.shed.is_empty() {
            GuardStatus::Idle
        } else {
            GuardStatus::Shedding
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::energy::Basis;
    use crate::peaks::LimitBasis;
    use crate::units::Celsius;
    use crate::zone::Override;
    use crate::zone::tests::{config, ts};

    const HOUR: &str = "2026-11-15T17:00:00Z";

    fn at(min: i64, sec: i64) -> Timestamp {
        ts(HOUR) + SignedDuration::from_mins(min) + SignedDuration::from_secs(sec)
    }

    fn meter(now: Timestamp, projected: f64) -> MeterSnapshot {
        MeterSnapshot {
            hour: HourStart::containing(now),
            used: Kwh::ZERO,
            basis: Basis::PowerOnly,
            power: Some(Kw::new(projected)),
            projected: Some(Kwh::new(projected)),
            freshness: Freshness::Fresh,
            last_seen: Some(now),
        }
    }

    fn limit(effective: f64) -> HourLimit {
        HourLimit {
            allowed: Kwh::new(effective + 0.3),
            effective: Kwh::new(effective),
            basis: LimitBasis::TierBound,
        }
    }

    /// Zones heating at `kw` each, with a base of 22 °C.
    fn zones(spec: &[(&str, u8, f64)]) -> BTreeMap<ZoneId, Zone> {
        spec.iter()
            .map(|&(id, prio, kw)| {
                let mut z = Zone::new(config(id, prio));
                z.base = Some(Celsius::new(22.0));
                z.power = Some((Kw::new(kw), ts(HOUR)));
                (z.id().clone(), z)
            })
            .collect()
    }

    struct Rig {
        cfg: GuardConfig,
        settings: ZoneSettings,
        zones: BTreeMap<ZoneId, Zone>,
        state: GuardState,
        limit: HourLimit,
    }

    impl Rig {
        fn new(spec: &[(&str, u8, f64)]) -> Self {
            Self {
                cfg: GuardConfig::default(),
                settings: ZoneSettings::default(),
                zones: zones(spec),
                state: GuardState::default(),
                limit: limit(4.7),
            }
        }

        fn step(&mut self, now: Timestamp, projected: f64) -> Vec<GuardEvent> {
            self.step_with(now, &meter(now, projected), true)
        }

        fn step_with(
            &mut self,
            now: Timestamp,
            m: &MeterSnapshot,
            enabled: bool,
        ) -> Vec<GuardEvent> {
            let input = GuardInput {
                now,
                meter: m,
                limit: &self.limit,
                zones: &self.zones,
                settings: &self.settings,
                enabled,
            };
            self.state.update(&self.cfg, &input)
        }

        fn shed_ids(&self) -> Vec<&str> {
            self.state.shed.iter().map(|s| s.zone.as_str()).collect()
        }

        fn stop_drawing(&mut self, id: &str) {
            let z = self.zones.get_mut(&ZoneId::new(id).unwrap()).unwrap();
            z.power = Some((Kw::new(0.01), ts(HOUR)));
        }
    }

    fn released(events: &[GuardEvent]) -> Vec<(&str, ReleaseReason)> {
        events
            .iter()
            .filter_map(|e| match e {
                GuardEvent::Release { zone, reason } => Some((zone.as_str(), *reason)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn default_config() {
        let c = GuardConfig::default();
        assert_eq!(c.release_margin, Kwh::new(0.3));
        assert_eq!(c.min_shed_duration, SignedDuration::from_mins(5));
        assert_eq!(c.restore_stagger, SignedDuration::from_mins(2));
        assert_eq!(c.settle, SignedDuration::from_secs(90));
    }

    #[test]
    fn quiet_hour_does_nothing() {
        let mut r = Rig::new(&[("bad", 1, 1.5)]);
        assert!(r.step(at(10, 0), 3.0).is_empty());
        assert_eq!(r.state.status, GuardStatus::Idle);
    }

    #[test]
    fn sheds_lowest_priority_number_first_and_only_as_many_as_needed() {
        let mut r = Rig::new(&[("kjeller", 6, 1.0), ("bad", 1, 1.0), ("gang", 3, 1.0)]);
        // At half past, 1 kW shed saves 0.5 kWh; excess 0.8 needs two zones.
        let ev = r.step(at(30, 0), 5.5);
        assert_eq!(r.shed_ids(), ["bad", "gang"]);
        assert_eq!(ev.len(), 2);
        assert!(released(&ev).is_empty());
        assert!(
            matches!(&ev[0], GuardEvent::Shed { zone, load, .. } if zone.as_str() == "bad" && *load == Kw::new(1.0))
        );
        assert_eq!(r.state.status, GuardStatus::Shedding);
        assert!(r.state.is_shed(&ZoneId::new("bad").unwrap()));
        assert!(
            r.state.shed[0]
                .reason
                .starts_with("projected 5.50 kWh > limit 4.70 kWh")
        );
    }

    #[test]
    fn late_spike_needs_more_zones_than_early_spike() {
        let spec = [("a", 1, 1.0), ("b", 2, 1.0), ("c", 3, 1.0), ("d", 4, 1.0)];
        let mut early = Rig::new(&spec);
        early.step(at(5, 0), 5.5);
        let mut late = Rig::new(&spec);
        late.step(at(55, 0), 5.5);
        assert_eq!(early.shed_ids().len(), 1);
        assert_eq!(
            late.shed_ids().len(),
            4,
            "little time left: shed everything"
        );
    }

    #[test]
    fn credit_prevents_shedding_again_before_the_meter_catches_up() {
        let mut r = Rig::new(&[("bad", 1, 1.0), ("gang", 3, 1.0)]);
        r.step(at(30, 0), 5.0);
        assert_eq!(r.shed_ids(), ["bad"]);
        // The real meter still shows the old projection, but bad's reduction is credited
        // (still drawing, and within the settle window).
        assert!(r.step(at(30, 10), 5.0).is_empty());
        assert_eq!(r.shed_ids(), ["bad"]);
        // Active mode: bad stops drawing; after `settle` the meter is trusted again.
        r.stop_drawing("bad");
        assert!(r.step(at(31, 0), 4.6).is_empty());
        assert_eq!(r.shed_ids(), ["bad"]);
    }

    #[test]
    fn observe_mode_banks_savings_of_released_zones_until_the_hour_ends() {
        let mut r = Rig::new(&[("bad", 1, 1.0)]);
        r.step(at(0, 0), 5.0);
        assert_eq!(r.shed_ids(), ["bad"]);
        // Still drawing (nothing was written). Released 30 min later; it virtually saved
        // 0.5 kWh, which the meter never saw.
        assert_eq!(
            released(&r.step(at(30, 0), 3.5)),
            [("bad", ReleaseReason::BelowLimit)]
        );
        assert_eq!(
            r.state.banked,
            Some((HourStart::containing(at(0, 0)), Kwh::new(0.5)))
        );
        assert!(
            r.step(at(31, 0), 5.1).is_empty(),
            "banked 0.5 keeps 5.1 under the limit"
        );
        // A new hour starts from scratch.
        r.step(at(60, 0), 1.0);
        assert_eq!(r.state.banked, None);
        // A zone that really switched off banks nothing.
        r.step(at(61, 0), 5.0);
        r.stop_drawing("bad");
        r.step(at(70, 0), 1.0);
        assert_eq!(r.state.banked, None);
    }

    #[test]
    fn idle_zones_are_not_shed() {
        let mut r = Rig::new(&[("bad", 1, 0.01), ("gang", 3, 1.0)]);
        r.step(at(30, 0), 5.0);
        assert_eq!(r.shed_ids(), ["gang"]);
    }

    #[test]
    fn unreachable_limit_sheds_everything_and_reports_once() {
        let mut r = Rig::new(&[("bad", 1, 1.0), ("gang", 3, 0.5)]);
        let ev = r.step(at(30, 0), 9.0);
        assert_eq!(r.shed_ids(), ["bad", "gang"]);
        assert!(
            matches!(ev.last(), Some(GuardEvent::LimitUnreachable { available, .. }) if (available.value() - 0.75).abs() < 1e-9)
        );
        assert_eq!(r.state.status, GuardStatus::LimitUnreachable);
        // Still over (e.g. EV charging), nothing left to shed: no repeated report.
        let ev = r.step(at(30, 10), 9.0 + 0.75);
        assert!(ev.is_empty());
        assert_eq!(r.state.status, GuardStatus::LimitUnreachable);
    }

    #[test]
    fn release_waits_for_hysteresis_minimum_duration_and_stagger() {
        let mut r = Rig::new(&[("bad", 1, 1.0), ("gang", 3, 1.0)]);
        r.step(at(10, 0), 6.5);
        assert_eq!(r.shed_ids(), ["bad", "gang"]);
        r.stop_drawing("bad");
        r.stop_drawing("gang");
        // Well below the limit, but not shed long enough yet.
        assert!(r.step(at(12, 0), 2.0).is_empty());
        // Long enough, but bringing gang back would exceed limit − margin.
        assert!(r.step(at(16, 0), 4.0).is_empty());
        assert_eq!(r.state.status, GuardStatus::Shedding);
        // Comfortable: release the most recently shed zone first.
        assert_eq!(
            released(&r.step(at(16, 0), 2.0)),
            [("gang", ReleaseReason::BelowLimit)]
        );
        // The next release waits for the stagger.
        assert!(r.step(at(17, 0), 2.0).is_empty());
        assert_eq!(
            released(&r.step(at(18, 0), 2.0)),
            [("bad", ReleaseReason::BelowLimit)]
        );
        assert_eq!(r.state.status, GuardStatus::Idle);
        assert!(r.step(at(20, 0), 2.0).is_empty());
    }

    #[test]
    fn hour_rollover_does_not_force_a_release() {
        let mut r = Rig::new(&[("bad", 1, 1.0)]);
        r.step(at(50, 0), 5.0);
        r.stop_drawing("bad");
        // New hour, load still high: the zone simply stays shed.
        let next = at(60, 10);
        assert!(r.step(next, 4.6).is_empty());
        assert_eq!(r.shed_ids(), ["bad"]);
    }

    #[test]
    fn stale_meter_releases_staggered_and_sheds_nothing() {
        let mut r = Rig::new(&[("bad", 1, 1.0), ("gang", 3, 1.0)]);
        r.step(at(30, 0), 6.0);
        assert_eq!(r.shed_ids(), ["bad", "gang"]);
        let mut stale = meter(at(31, 0), 9.0);
        stale.freshness = Freshness::Stale;
        let ev = r.step_with(at(31, 0), &stale, true);
        assert_eq!(released(&ev), [("gang", ReleaseReason::MeterStale)]);
        assert_eq!(r.state.status, GuardStatus::MeterStale);
        assert!(r.step_with(at(32, 0), &stale, true).is_empty());
        stale.freshness = Freshness::NoData;
        assert_eq!(
            released(&r.step_with(at(33, 0), &stale, true)),
            [("bad", ReleaseReason::MeterStale)]
        );
    }

    #[test]
    fn override_or_disabled_shedding_releases_immediately() {
        let mut r = Rig::new(&[("bad", 1, 1.0), ("gang", 3, 1.0)]);
        r.step(at(30, 0), 6.0);
        let gang = ZoneId::new("gang").unwrap();
        r.zones.get_mut(&gang).unwrap().manual = Some(Override {
            value: Celsius::new(25.0),
            until: at(150, 0),
        });
        r.zones
            .get_mut(&ZoneId::new("bad").unwrap())
            .unwrap()
            .shed_allowed_override = Some(false);
        let ev = r.step(at(30, 10), 6.0);
        assert_eq!(
            released(&ev),
            [
                ("bad", ReleaseReason::NotSheddable),
                ("gang", ReleaseReason::NotSheddable)
            ]
        );
        assert!(r.state.shed.is_empty());
    }

    #[test]
    fn unknown_zone_is_released() {
        let mut r = Rig::new(&[("bad", 1, 1.0)]);
        r.state.shed.push(Shed {
            zone: ZoneId::new("removed").unwrap(),
            since: at(0, 0),
            load: Kw::new(1.0),
            reason: String::new(),
        });
        assert_eq!(
            released(&r.step(at(1, 0), 1.0)),
            [("removed", ReleaseReason::NotSheddable)]
        );
    }

    #[test]
    fn turning_the_guard_off_releases_everything_at_once() {
        let mut r = Rig::new(&[("bad", 1, 1.0), ("gang", 3, 1.0)]);
        r.step(at(30, 0), 6.0);
        let m = meter(at(31, 0), 6.0);
        let ev = r.step_with(at(31, 0), &m, false);
        assert_eq!(
            released(&ev),
            [
                ("bad", ReleaseReason::GuardOff),
                ("gang", ReleaseReason::GuardOff)
            ]
        );
        assert_eq!(r.state.status, GuardStatus::Disabled);
    }

    proptest! {
        /// Zones that may not be shed are never in the shed set, whatever the load does.
        #[test]
        fn never_sheds_a_protected_zone(
            zones in prop::collection::vec((any::<bool>(), any::<bool>(), 0.0f64..3.0, 0u8..10), 1..6),
            loads in prop::collection::vec(0.0f64..15.0, 1..40),
        ) {
            let spec: Vec<(String, u8, f64)> = zones
                .iter()
                .enumerate()
                .map(|(i, &(_, _, kw, prio))| (format!("z{i}"), prio, kw))
                .collect();
            let spec_ref: Vec<(&str, u8, f64)> = spec.iter().map(|(s, p, k)| (s.as_str(), *p, *k)).collect();
            let mut r = Rig::new(&spec_ref);
            for (i, &(allowed, overridden, _, _)) in zones.iter().enumerate() {
                let z = r.zones.get_mut(&ZoneId::new(&format!("z{i}")).unwrap()).unwrap();
                z.shed_allowed_override = Some(allowed);
                if overridden {
                    z.manual = Some(Override { value: Celsius::new(23.0), until: at(600, 0) });
                }
            }
            for (i, load) in loads.iter().enumerate() {
                let now = at(0, 0) + SignedDuration::from_secs(i64::try_from(i).unwrap() * 30);
                r.step(now, *load);
                for s in &r.state.shed {
                    let z = &r.zones[&s.zone];
                    prop_assert!(z.shed_allowed() && z.active_override(now).is_none());
                }
            }
        }
    }
}

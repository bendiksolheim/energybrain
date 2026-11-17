//! Energy accounting for the whole-house meter.
//!
//! Two kinds of reading come in:
//! - **instant power** every few seconds, integrated (trapezoidal) for the running hour;
//! - the **cumulative import register**, normally once per hour, stamped by the meter at the
//!   hour boundary. This is what the grid company bills on.
//!
//! When an hour ends, [`Meter::advance`] emits an `Estimated` value from the power integral.
//! When register values are known at both of an hour's boundaries,
//! [`Meter::record_cumulative`] emits the billing-grade value (`Measured`, or `Interpolated`
//! when a boundary value had to be interpolated between two surrounding readings). Later
//! values for the same hour supersede earlier ones; consumers upsert by hour.

use std::collections::VecDeque;
use std::iter::once;

use jiff::{SignedDuration, Timestamp};

use crate::time::HourStart;
use crate::units::{Kw, Kwh};

/// Tuning for the meter model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterConfig {
    /// Length of the moving average used as "current power" for projections, so a kettle
    /// spike does not trigger shedding.
    pub power_window: SignedDuration,
    /// With no power reading for this long the meter is considered stale.
    pub stale_after: SignedDuration,
}

impl Default for MeterConfig {
    fn default() -> Self {
        Self {
            power_window: SignedDuration::from_secs(60),
            stale_after: SignedDuration::from_secs(120),
        }
    }
}

/// How an hour's energy was determined, worst to best.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// From power integration only.
    Estimated,
    /// From the register, with at least one boundary value interpolated.
    Interpolated,
    /// From register values exactly at both boundaries.
    Measured,
}

/// The energy used in one hour.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct HourEnergy {
    /// The hour.
    pub hour: HourStart,
    /// Energy imported during it (`energy` in JSON, where the unit is part of the value).
    #[serde(rename = "energy")]
    pub kwh: Kwh,
    /// How it was determined.
    pub quality: Quality,
}

/// What the running hour's `used` figure is based on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Register value at the hour start (plus power integration since the last reading).
    Register,
    /// Power integration only.
    PowerOnly,
}

/// Whether instant-power data is current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// No power reading received yet.
    NoData,
    /// The last power reading is recent.
    Fresh,
    /// The last power reading is older than `stale_after`.
    Stale,
}

/// The meter's view of the running hour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeterSnapshot {
    /// The running hour.
    pub hour: HourStart,
    /// Energy used so far this hour.
    pub used: Kwh,
    /// What `used` is based on.
    pub basis: Basis,
    /// Moving-average power, if any power reading exists.
    pub power: Option<Kw>,
    /// `used` plus `power` sustained to the end of the hour.
    pub projected: Option<Kwh>,
    /// Data freshness.
    pub freshness: Freshness,
    /// When the last power reading was taken.
    pub last_seen: Option<Timestamp>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Reading {
    at: Timestamp,
    total: Kwh,
}

/// Register value at an hour's start.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Boundary {
    hour: HourStart,
    total: Kwh,
    quality: Quality,
}

/// The whole-house meter model.
#[derive(Debug, Clone)]
pub struct Meter {
    config: MeterConfig,
    /// Power readings, strictly increasing in time. Holds the running hour, the moving
    /// average window and one reading before both (for interpolation at their start).
    samples: VecDeque<(Timestamp, Kw)>,
    last_reading: Option<Reading>,
    last_boundary: Option<Boundary>,
    open_hour: Option<HourStart>,
    /// Latest hour already accounted from the register, so its estimate is not emitted.
    last_register_hour: Option<HourStart>,
}

impl Meter {
    /// An empty meter.
    pub fn new(config: MeterConfig) -> Self {
        Self {
            config,
            samples: VecDeque::new(),
            last_reading: None,
            last_boundary: None,
            open_hour: None,
            last_register_hour: None,
        }
    }

    /// Records an instant-power reading. Readings not newer than the last one are ignored.
    pub fn record_power(&mut self, at: Timestamp, power: Kw) {
        if self.samples.back().is_some_and(|&(last, _)| at <= last) {
            return;
        }
        self.samples.push_back((at, power));
        self.prune();
    }

    /// Records a cumulative-register reading and returns any hours it settles.
    ///
    /// Readings not newer than the last one are ignored. A register that goes backwards
    /// (meter replaced) restarts the register history.
    pub fn record_cumulative(&mut self, at: Timestamp, total: Kwh) -> Vec<HourEnergy> {
        let mut settled = Vec::new();
        match self.last_reading {
            Some(prev) if at <= prev.at => return settled,
            Some(prev) if total >= prev.total => {
                let span = at.duration_since(prev.at).as_secs_f64();
                let mut b = HourStart::containing(prev.at).next();
                while b.start() <= at {
                    let frac = b.start().duration_since(prev.at).as_secs_f64() / span;
                    let quality = if b.start() == at {
                        Quality::Measured
                    } else {
                        Quality::Interpolated
                    };
                    let boundary = Boundary {
                        hour: b,
                        total: prev.total + (total - prev.total) * frac,
                        quality,
                    };
                    settled.extend(self.set_boundary(boundary));
                    b = b.next();
                }
            }
            _ => {
                let hour = HourStart::containing(at);
                self.last_boundary = (hour.start() == at).then_some(Boundary {
                    hour,
                    total,
                    quality: Quality::Measured,
                });
            }
        }
        self.last_reading = Some(Reading { at, total });
        settled
    }

    /// Moves the running hour to the one containing `now`. If an hour just ended and the
    /// register has not already settled it, returns its estimate.
    pub fn advance(&mut self, now: Timestamp) -> Option<HourEnergy> {
        let current = HourStart::containing(now);
        let ended = match self.open_hour {
            Some(h) if current > h => h,
            Some(_) => return None,
            None => {
                self.open_hour = Some(current);
                return None;
            }
        };
        let estimate = (self.last_register_hour != Some(ended)).then(|| HourEnergy {
            hour: ended,
            kwh: self.used_between(ended, ended.end()).0,
            quality: Quality::Estimated,
        });
        self.open_hour = Some(current);
        self.prune();
        estimate
    }

    /// The running hour as of `now`.
    pub fn snapshot(&self, now: Timestamp) -> MeterSnapshot {
        let hour = HourStart::containing(now);
        let last_seen = self.samples.back().map(|&(t, _)| t);
        let upto = last_seen.map_or(hour.start(), |t| t.min(now).max(hour.start()));
        let (used, basis) = self.used_between(hour, upto);
        let window = self.config.power_window;
        let power = last_seen.map(|t| self.integrate(t - window, t) / window);
        let projected = power.map(|p| used + p * hour.end().duration_since(upto));
        let freshness = match last_seen {
            None => Freshness::NoData,
            Some(t) if now.duration_since(t) > self.config.stale_after => Freshness::Stale,
            Some(_) => Freshness::Fresh,
        };
        MeterSnapshot {
            hour,
            used,
            basis,
            power,
            projected,
            freshness,
            last_seen,
        }
    }

    /// Stores a new boundary value; if the previous one was the hour before, settles it.
    fn set_boundary(&mut self, boundary: Boundary) -> Option<HourEnergy> {
        let prev = self
            .last_boundary
            .replace(boundary)
            .filter(|p| p.hour.next() == boundary.hour)?;
        self.last_register_hour = Some(prev.hour);
        Some(HourEnergy {
            hour: prev.hour,
            kwh: boundary.total - prev.total,
            quality: prev.quality.min(boundary.quality),
        })
    }

    /// Energy used in `hour` from its start up to `upto`.
    fn used_between(&self, hour: HourStart, upto: Timestamp) -> (Kwh, Basis) {
        match (self.last_boundary, self.last_reading) {
            (Some(b), Some(r)) if b.hour == hour && r.at <= upto => (
                (r.total - b.total) + self.integrate(r.at, upto),
                Basis::Register,
            ),
            _ => (self.integrate(hour.start(), upto), Basis::PowerOnly),
        }
    }

    /// Integral of the power curve over `[from, to]`. The curve is linear between readings
    /// and held flat before the first and after the last reading.
    fn integrate(&self, from: Timestamp, to: Timestamp) -> Kwh {
        if to <= from || self.samples.is_empty() {
            return Kwh::ZERO;
        }
        let inner = self
            .samples
            .iter()
            .filter(|&&(t, _)| from < t && t < to)
            .copied();
        let start = (from, self.power_at(from));
        let end = (to, self.power_at(to));
        let (_, total) = inner
            .chain(once(end))
            .fold((start, Kwh::ZERO), |((t0, p0), acc), (t1, p1)| {
                ((t1, p1), acc + (p0 + p1) / 2.0 * t1.duration_since(t0))
            });
        total
    }

    /// The power curve's value at `t`.
    fn power_at(&self, t: Timestamp) -> Kw {
        let i = self.samples.partition_point(|&(st, _)| st <= t);
        let before = i.checked_sub(1).and_then(|j| self.samples.get(j)).copied();
        let after = self.samples.get(i).copied();
        match (before, after) {
            (Some((t0, p0)), Some((t1, p1))) => {
                let frac = t.duration_since(t0).as_secs_f64() / t1.duration_since(t0).as_secs_f64();
                p0 + (p1 - p0) * frac
            }
            (before, after) => before.or(after).map_or(Kw::ZERO, |(_, p)| p),
        }
    }

    /// Drops readings no longer needed for the running hour or the moving average.
    fn prune(&mut self) {
        let Some(&(last, _)) = self.samples.back() else {
            return;
        };
        let window_start = last - self.config.power_window;
        let keep_from = self
            .open_hour
            .map_or(window_start, |h| window_start.min(h.start()));
        while self.samples.get(1).is_some_and(|&(t, _)| t <= keep_from) {
            self.samples.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn secs(n: i64) -> SignedDuration {
        SignedDuration::from_secs(n)
    }

    fn close(a: Kwh, b: f64) -> bool {
        (a.value() - b).abs() < 1e-9
    }

    /// Feeds constant power every 2 s over `[from, from + dur)`.
    fn feed(m: &mut Meter, from: Timestamp, dur: SignedDuration, kw: f64) {
        let mut t = from;
        while t < from + dur {
            m.record_power(t, Kw::new(kw));
            t += secs(2);
        }
    }

    #[test]
    fn default_config() {
        let c = MeterConfig::default();
        assert_eq!(c.power_window, secs(60));
        assert_eq!(c.stale_after, secs(120));
    }

    #[test]
    fn empty_meter() {
        let m = Meter::new(MeterConfig::default());
        let s = m.snapshot(ts("2026-11-15T17:20:00Z"));
        assert_eq!(s.hour, HourStart::containing(ts("2026-11-15T17:00:00Z")));
        assert_eq!(s.used, Kwh::ZERO);
        assert_eq!(s.basis, Basis::PowerOnly);
        assert_eq!(s.power, None);
        assert_eq!(s.projected, None);
        assert_eq!(s.freshness, Freshness::NoData);
        assert_eq!(s.last_seen, None);
    }

    #[test]
    fn constant_power_projection() {
        let mut m = Meter::new(MeterConfig::default());
        let start = ts("2026-11-15T17:00:00Z");
        m.advance(start);
        feed(&mut m, start, SignedDuration::from_mins(15) + secs(2), 6.0);
        let now = ts("2026-11-15T17:15:00Z");
        let s = m.snapshot(now);
        assert!(close(s.used, 1.5), "{:?}", s.used);
        assert!(close(s.power.unwrap().for_one_hour(), 6.0));
        assert!(close(s.projected.unwrap(), 6.0));
        assert_eq!(s.freshness, Freshness::Fresh);
        assert_eq!(s.last_seen, Some(now));
    }

    #[test]
    fn goes_stale_without_power_readings() {
        let mut m = Meter::new(MeterConfig::default());
        let t = ts("2026-11-15T17:00:00Z");
        m.record_power(t, Kw::new(1.0));
        assert_eq!(m.snapshot(t + secs(120)).freshness, Freshness::Fresh);
        assert_eq!(m.snapshot(t + secs(121)).freshness, Freshness::Stale);
    }

    #[test]
    fn moving_average_damps_a_kettle() {
        let mut m = Meter::new(MeterConfig::default());
        let t = ts("2026-11-15T17:10:00Z");
        feed(&mut m, t, secs(120), 3.0);
        // A 2 kW kettle for 20 s, right at the end.
        feed(&mut m, t + secs(120), secs(20), 5.0);
        let p = m.snapshot(t + secs(138)).power.unwrap().value();
        assert!(
            p > 3.0 && p < 4.0,
            "kettle should be averaged down, got {p}"
        );
    }

    #[test]
    fn mid_hour_start_backfills_with_first_reading() {
        let mut m = Meter::new(MeterConfig::default());
        let t = ts("2026-11-15T17:30:00Z");
        m.advance(t);
        m.record_power(t, Kw::new(4.0));
        let s = m.snapshot(t);
        assert!(close(s.used, 2.0));
        assert!(close(s.projected.unwrap(), 4.0));
    }

    #[test]
    fn stale_reading_before_the_hour_counts_nothing_yet() {
        let mut m = Meter::new(MeterConfig::default());
        m.record_power(ts("2026-11-15T16:50:00Z"), Kw::new(4.0));
        let s = m.snapshot(ts("2026-11-15T17:05:00Z"));
        assert_eq!(s.used, Kwh::ZERO);
        assert_eq!(s.freshness, Freshness::Stale);
    }

    #[test]
    fn out_of_order_power_reading_is_ignored() {
        let mut m = Meter::new(MeterConfig::default());
        let t = ts("2026-11-15T17:00:00Z");
        m.record_power(t, Kw::new(1.0));
        m.record_power(t, Kw::new(9.0));
        m.record_power(t - secs(1), Kw::new(9.0));
        assert_eq!(m.snapshot(t).power, Some(Kw::new(1.0)));
    }

    #[test]
    fn hour_close_emits_estimate_then_register_settles_it() {
        let mut m = Meter::new(MeterConfig::default());
        let h0 = ts("2026-11-15T17:00:00Z");
        let h1 = ts("2026-11-15T18:00:00Z");
        assert_eq!(
            m.record_cumulative(h0, Kwh::new(1000.0)),
            Vec::<HourEnergy>::new()
        );
        m.advance(h0);
        feed(&mut m, h0, SignedDuration::from_hours(1), 4.0);

        let est = m.advance(h1 + secs(1)).unwrap();
        assert_eq!(est.hour, HourStart::containing(h0));
        assert_eq!(est.quality, Quality::Estimated);
        assert!(close(est.kwh, 4.0));
        assert_eq!(m.advance(h1 + secs(5)), None, "same hour: nothing new");

        let settled = m.record_cumulative(h1, Kwh::new(1004.2));
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].hour, HourStart::containing(h0));
        assert_eq!(settled[0].quality, Quality::Measured);
        assert!(close(settled[0].kwh, 4.2));
    }

    #[test]
    fn register_ahead_of_clock_suppresses_the_estimate() {
        let mut m = Meter::new(MeterConfig::default());
        let h0 = ts("2026-11-15T17:00:00Z");
        let h1 = ts("2026-11-15T18:00:00Z");
        m.advance(h0);
        m.record_cumulative(h0, Kwh::new(1000.0));
        assert_eq!(m.record_cumulative(h1, Kwh::new(1003.0)).len(), 1);
        assert_eq!(m.advance(h1 + secs(2)), None);
    }

    #[test]
    fn late_reading_is_interpolated() {
        let mut m = Meter::new(MeterConfig::default());
        m.record_cumulative(ts("2026-11-15T17:00:00Z"), Kwh::new(1000.0));
        // Next reading 30 min late; register rose 6 kWh over 1.5 h.
        let settled = m.record_cumulative(ts("2026-11-15T18:30:00Z"), Kwh::new(1006.0));
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].quality, Quality::Interpolated);
        assert!(close(settled[0].kwh, 4.0));
    }

    #[test]
    fn long_gap_settles_every_hour_in_between() {
        let mut m = Meter::new(MeterConfig::default());
        m.record_cumulative(ts("2026-11-15T17:00:00Z"), Kwh::new(1000.0));
        let settled = m.record_cumulative(ts("2026-11-15T20:00:00Z"), Kwh::new(1009.0));
        let hours: Vec<_> = settled.iter().map(|e| e.hour.start()).collect();
        assert_eq!(
            hours,
            [
                ts("2026-11-15T17:00:00Z"),
                ts("2026-11-15T18:00:00Z"),
                ts("2026-11-15T19:00:00Z")
            ]
        );
        assert!(settled.iter().all(|e| close(e.kwh, 3.0)));
        assert_eq!(
            settled.iter().map(|e| e.quality).collect::<Vec<_>>(),
            [
                Quality::Interpolated,
                Quality::Interpolated,
                Quality::Interpolated
            ]
        );
    }

    #[test]
    fn unaligned_first_reading_settles_nothing_until_two_boundaries_are_known() {
        let mut m = Meter::new(MeterConfig::default());
        m.record_cumulative(ts("2026-11-15T17:30:00Z"), Kwh::new(1000.0));
        assert_eq!(
            m.record_cumulative(ts("2026-11-15T18:30:00Z"), Kwh::new(1002.0)),
            Vec::<HourEnergy>::new()
        );
        let settled = m.record_cumulative(ts("2026-11-15T19:00:00Z"), Kwh::new(1003.0));
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].hour.start(), ts("2026-11-15T18:00:00Z"));
        assert!(close(settled[0].kwh, 2.0));
        assert_eq!(settled[0].quality, Quality::Interpolated);
    }

    #[test]
    fn stale_or_backwards_readings() {
        let mut m = Meter::new(MeterConfig::default());
        let h0 = ts("2026-11-15T17:00:00Z");
        m.record_cumulative(h0, Kwh::new(1000.0));
        assert_eq!(
            m.record_cumulative(h0, Kwh::new(1001.0)),
            Vec::<HourEnergy>::new()
        );
        assert_eq!(
            m.record_cumulative(h0 - secs(1), Kwh::new(1001.0)),
            Vec::<HourEnergy>::new()
        );
        // Meter replaced: register restarts low, history restarts.
        let h1 = ts("2026-11-15T18:00:00Z");
        assert_eq!(
            m.record_cumulative(h1, Kwh::new(5.0)),
            Vec::<HourEnergy>::new()
        );
        let settled = m.record_cumulative(ts("2026-11-15T19:00:00Z"), Kwh::new(8.0));
        assert_eq!(settled.len(), 1);
        assert!(close(settled[0].kwh, 3.0));
    }

    #[test]
    fn register_basis_mid_hour() {
        let mut m = Meter::new(MeterConfig::default());
        let h0 = ts("2026-11-15T17:00:00Z");
        m.advance(h0);
        m.record_cumulative(h0, Kwh::new(1000.0));
        // A mid-hour register reading (unusual but possible) anchors `used`.
        let mid = ts("2026-11-15T17:20:00Z");
        m.record_cumulative(mid, Kwh::new(1001.0));
        feed(&mut m, mid, SignedDuration::from_mins(10) + secs(2), 3.0);
        let s = m.snapshot(ts("2026-11-15T17:30:00Z"));
        assert_eq!(s.basis, Basis::Register);
        assert!(close(s.used, 1.5));
        assert!(close(s.projected.unwrap(), 3.0));
    }

    #[test]
    fn clock_going_backwards_keeps_the_hour() {
        let mut m = Meter::new(MeterConfig::default());
        m.advance(ts("2026-11-15T18:00:00Z"));
        assert_eq!(m.advance(ts("2026-11-15T17:59:00Z")), None);
        assert_eq!(m.open_hour.unwrap().start(), ts("2026-11-15T18:00:00Z"));
    }

    #[test]
    fn advancing_an_empty_meter_estimates_zero() {
        let mut m = Meter::new(MeterConfig::default());
        m.advance(ts("2026-11-15T17:00:00Z"));
        let est = m.advance(ts("2026-11-15T18:00:00Z")).unwrap();
        assert_eq!(est.kwh, Kwh::ZERO);
    }

    #[test]
    fn pruning_keeps_the_running_hour() {
        let mut m = Meter::new(MeterConfig::default());
        let h0 = ts("2026-11-15T17:00:00Z");
        m.advance(h0);
        feed(
            &mut m,
            h0 - SignedDuration::from_mins(5),
            SignedDuration::from_mins(65),
            2.0,
        );
        // Previous-hour readings are gone except the one at or before the hour start.
        assert!(m.samples[0].0 <= h0 && m.samples[1].0 > h0);
        assert!(close(
            m.snapshot(ts("2026-11-15T17:59:58Z")).used,
            2.0 * 3598.0 / 3600.0
        ));
    }

    proptest! {
        /// However readings are spaced, the settled hours add up to the register delta.
        #[test]
        fn settled_hours_sum_to_register_delta(
            steps in prop::collection::vec((60i64..10_000, 0.0f64..20.0), 1..40),
        ) {
            let mut m = Meter::new(MeterConfig::default());
            let start = ts("2026-11-15T00:00:00Z");
            let mut t = start;
            let mut total = 1000.0;
            m.record_cumulative(t, Kwh::new(total));
            let mut latest = std::collections::BTreeMap::new();
            for (dt, de) in steps {
                t += secs(dt);
                total += de;
                for e in m.record_cumulative(t, Kwh::new(total)) {
                    latest.insert(e.hour, e.kwh);
                }
            }
            // Close on an exact boundary so the last partial hour is settled too.
            let end = HourStart::containing(t).next().start();
            let end_total = total + 1.0;
            for e in m.record_cumulative(end, Kwh::new(end_total)) {
                latest.insert(e.hour, e.kwh);
            }
            let hours = end.duration_since(start).as_hours();
            prop_assert_eq!(i64::try_from(latest.len()).unwrap(), hours);
            let sum: Kwh = latest.values().copied().sum();
            prop_assert!((sum.value() - (end_total - 1000.0)).abs() < 1e-6);
        }
    }
}

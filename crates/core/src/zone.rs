//! Floor-heating zones: configuration plus what we have learned about each one.

use jiff::{SignedDuration, Timestamp};

use crate::ids::ZoneId;
use crate::time::HourStart;
use crate::units::{Celsius, CelsiusDelta, Kw, Kwh};

/// Static configuration of one zone.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneConfig {
    /// Identifier, used in topics and storage.
    pub id: ZoneId,
    /// Human-readable name.
    pub name: String,
    /// Shed order: **lower numbers shed first** and are restored last.
    pub priority: u8,
    /// How far below its base the zone is set while shed.
    pub shed_delta: CelsiusDelta,
    /// Lowest setpoint ever written.
    pub min_setpoint: Celsius,
    /// Highest setpoint ever written.
    pub max_setpoint: Celsius,
    /// Whether the guard may shed this zone (default; HomeKit can override at runtime).
    pub shed_allowed: bool,
    /// Load estimate before anything has been measured or learned.
    pub nominal_kw: Kw,
}

/// Settings shared by all zones.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneSettings {
    /// A report within this distance of our last write is an echo of it.
    pub echo_tolerance: CelsiusDelta,
    /// How long a manual override keeps a zone out of shedding.
    pub override_duration: SignedDuration,
    /// Power above this counts as "heating".
    pub heating_threshold: Kw,
    /// Weight of a new power report in the learned heating power (0–1).
    pub learn_rate: f64,
}

impl Default for ZoneSettings {
    fn default() -> Self {
        Self {
            echo_tolerance: CelsiusDelta::new(0.25),
            override_duration: SignedDuration::from_hours(2),
            heating_threshold: Kw::new(0.05),
            learn_rate: 0.2,
        }
    }
}

/// A user override: the zone is kept at `value` and not shed until `until`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Override {
    /// The user's setpoint.
    pub value: Celsius,
    /// When the override ends.
    pub until: Timestamp,
}

/// How a reported setpoint was interpreted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SetpointReport {
    /// It matches our last write.
    Echo,
    /// The user changed the setpoint; it is the new base.
    NewBase(Celsius),
    /// The user raised the setpoint above the shed cap: new base and an override.
    Override(Override),
}

/// A zone and everything known about it at runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct Zone {
    /// Static configuration.
    pub config: ZoneConfig,
    /// The setpoint the user wants (learned from non-echo reports; persisted).
    pub base: Option<Celsius>,
    /// The thermostat's last reported setpoint.
    pub reported: Option<Celsius>,
    /// Our last real write (persisted).
    pub last_write: Option<(Celsius, Timestamp)>,
    /// Our last "would write" in observe mode, standing in for the thermostat's state.
    pub last_would_write: Option<(Celsius, Timestamp)>,
    /// Last power report.
    pub power: Option<(Kw, Timestamp)>,
    /// Learned power while heating (persisted).
    pub heating_kw: Option<Kw>,
    /// Runtime shed permission from HomeKit (persisted); wins over the config default.
    pub shed_allowed_override: Option<bool>,
    /// Active or expired manual override (persisted).
    pub manual: Option<Override>,
    /// The thermostat's kWh counter at the first report of an hour.
    energy_mark: Option<(HourStart, Kwh)>,
}

impl Zone {
    /// A zone with nothing learned yet.
    pub fn new(config: ZoneConfig) -> Self {
        Self {
            config,
            base: None,
            reported: None,
            last_write: None,
            last_would_write: None,
            power: None,
            heating_kw: None,
            shed_allowed_override: None,
            manual: None,
            energy_mark: None,
        }
    }

    /// The zone id.
    pub fn id(&self) -> &ZoneId {
        &self.config.id
    }

    /// Whether shedding is currently permitted for this zone.
    pub fn shed_allowed(&self) -> bool {
        self.shed_allowed_override
            .unwrap_or(self.config.shed_allowed)
    }

    /// The override in force at `now`, if any.
    pub fn active_override(&self, now: Timestamp) -> Option<Override> {
        self.manual.filter(|o| o.until > now)
    }

    /// Whether the guard may shed this zone at `now`: permitted, base known, no override.
    pub fn sheddable(&self, now: Timestamp) -> bool {
        self.shed_allowed() && self.base.is_some() && self.active_override(now).is_none()
    }

    /// The setpoint while shed, if the base is known.
    pub fn shed_cap(&self) -> Option<Celsius> {
        self.base
            .map(|b| (b - self.config.shed_delta).max(self.config.min_setpoint))
    }

    /// Best current estimate of the zone's load: last power report, else learned heating
    /// power, else the configured nominal power.
    pub fn load(&self) -> Kw {
        self.power
            .map(|(p, _)| p)
            .or(self.heating_kw)
            .unwrap_or(self.config.nominal_kw)
    }

    /// Whether the last power report shows the zone heating.
    pub fn is_drawing(&self, settings: &ZoneSettings) -> bool {
        self.power
            .is_some_and(|(p, _)| p > settings.heating_threshold)
    }

    /// Interprets a setpoint report. `cap` is the shed cap if the zone is currently shed.
    pub fn on_setpoint(
        &mut self,
        at: Timestamp,
        value: Celsius,
        cap: Option<Celsius>,
        settings: &ZoneSettings,
    ) -> SetpointReport {
        self.reported = Some(value);
        let tol = settings.echo_tolerance;
        if self
            .last_write
            .is_some_and(|(w, _)| (value - w).abs() <= tol)
        {
            return SetpointReport::Echo;
        }
        self.base = Some(value);
        self.last_would_write = None;
        match cap {
            Some(c) if value > c + tol => {
                let o = Override {
                    value,
                    until: at + settings.override_duration,
                };
                self.manual = Some(o);
                SetpointReport::Override(o)
            }
            _ => SetpointReport::NewBase(value),
        }
    }

    /// Records a power report. Returns the updated learned heating power if it changed.
    pub fn on_power(&mut self, at: Timestamp, power: Kw, settings: &ZoneSettings) -> Option<Kw> {
        self.power = Some((power, at));
        if power <= settings.heating_threshold {
            return None;
        }
        let learned = self.heating_kw.map_or(power, |old| {
            old * (1.0 - settings.learn_rate) + power * settings.learn_rate
        });
        self.heating_kw = Some(learned);
        Some(learned)
    }

    /// Records the thermostat's kWh counter. When the first report of a new hour arrives
    /// right after the previous hour, returns that previous hour's energy.
    pub fn on_energy(&mut self, at: Timestamp, total: Kwh) -> Option<(HourStart, Kwh)> {
        let hour = HourStart::containing(at);
        match self.energy_mark {
            Some((mark, _)) if hour <= mark => None,
            Some((mark, mark_total)) => {
                self.energy_mark = Some((hour, total));
                (hour == mark.next() && total >= mark_total).then(|| (mark, total - mark_total))
            }
            None => {
                self.energy_mark = Some((hour, total));
                None
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    pub(crate) fn config(id: &str, priority: u8) -> ZoneConfig {
        ZoneConfig {
            id: ZoneId::new(id).unwrap(),
            name: id.to_owned(),
            priority,
            shed_delta: CelsiusDelta::new(4.0),
            min_setpoint: Celsius::new(5.0),
            max_setpoint: Celsius::new(35.0),
            shed_allowed: true,
            nominal_kw: Kw::new(1.0),
        }
    }

    fn c(v: f64) -> Celsius {
        Celsius::new(v)
    }

    #[test]
    fn first_report_is_the_base() {
        let s = ZoneSettings::default();
        let mut z = Zone::new(config("bad", 1));
        assert_eq!(z.id().as_str(), "bad");
        let t = ts("2026-11-15T17:00:00Z");
        assert_eq!(
            z.on_setpoint(t, c(26.5), None, &s),
            SetpointReport::NewBase(c(26.5))
        );
        assert_eq!(z.base, Some(c(26.5)));
        assert_eq!(z.reported, Some(c(26.5)));
        assert_eq!(z.shed_cap(), Some(c(22.5)));
    }

    #[test]
    fn echoes_never_change_the_base_however_late() {
        let s = ZoneSettings::default();
        let mut z = Zone::new(config("bad", 1));
        z.base = Some(c(26.5));
        z.last_write = Some((c(22.5), ts("2026-11-15T17:00:00Z")));
        // Hours later (e.g. after a restart) the shed value is still an echo.
        let late = ts("2026-11-15T23:00:00Z");
        assert_eq!(z.on_setpoint(late, c(22.6), None, &s), SetpointReport::Echo);
        assert_eq!(z.base, Some(c(26.5)));
        assert_eq!(z.reported, Some(c(22.6)));
    }

    #[test]
    fn raising_above_the_cap_while_shed_is_an_override() {
        let s = ZoneSettings::default();
        let mut z = Zone::new(config("bad", 1));
        z.base = Some(c(26.5));
        z.last_write = Some((c(22.5), ts("2026-11-15T17:00:00Z")));
        z.last_would_write = Some((c(22.5), ts("2026-11-15T17:00:00Z")));
        let t = ts("2026-11-15T17:10:00Z");
        let o = Override {
            value: c(27.0),
            until: ts("2026-11-15T19:10:00Z"),
        };
        assert_eq!(
            z.on_setpoint(t, c(27.0), Some(c(22.5)), &s),
            SetpointReport::Override(o)
        );
        assert_eq!(z.base, Some(c(27.0)));
        assert_eq!(z.last_would_write, None);
        assert_eq!(z.active_override(t), Some(o));
        assert!(!z.sheddable(t));
        // Once expired the zone is sheddable again and keeps the value as base.
        assert_eq!(z.active_override(o.until), None);
        assert!(z.sheddable(o.until));
        assert_eq!(z.base, Some(c(27.0)));
    }

    #[test]
    fn lowering_while_shed_just_moves_the_base() {
        let s = ZoneSettings::default();
        let mut z = Zone::new(config("bad", 1));
        z.base = Some(c(26.5));
        z.last_write = Some((c(22.5), ts("2026-11-15T17:00:00Z")));
        let r = z.on_setpoint(ts("2026-11-15T17:10:00Z"), c(21.0), Some(c(22.5)), &s);
        assert_eq!(r, SetpointReport::NewBase(c(21.0)));
        assert_eq!(z.manual, None);
        assert_eq!(z.shed_cap(), Some(c(17.0)));
    }

    #[test]
    fn cap_is_clamped_to_min_setpoint() {
        let mut z = Zone::new(config("kjeller", 6));
        z.base = Some(c(7.0));
        assert_eq!(z.shed_cap(), Some(c(5.0)));
        z.base = None;
        assert_eq!(z.shed_cap(), None);
    }

    #[test]
    fn shed_permission() {
        let t = ts("2026-11-15T17:00:00Z");
        let mut z = Zone::new(config("utleie_1", 4));
        assert!(!z.sheddable(t), "no base yet");
        z.base = Some(c(18.0));
        assert!(z.sheddable(t));
        z.shed_allowed_override = Some(false);
        assert!(!z.shed_allowed());
        assert!(!z.sheddable(t));
        z.config.shed_allowed = false;
        z.shed_allowed_override = Some(true);
        assert!(z.sheddable(t), "runtime switch wins over config");
    }

    #[test]
    fn load_estimate_prefers_measurement_then_learning_then_nominal() {
        let s = ZoneSettings::default();
        let mut z = Zone::new(config("bad", 1));
        assert_eq!(z.load(), Kw::new(1.0));
        z.heating_kw = Some(Kw::new(1.8));
        assert_eq!(z.load(), Kw::new(1.8));
        assert!(!z.is_drawing(&s));
        z.on_power(ts("2026-11-15T17:00:00Z"), Kw::new(0.01), &s);
        assert_eq!(z.load(), Kw::new(0.01), "idle zone contributes nothing");
        assert!(!z.is_drawing(&s));
    }

    #[test]
    fn learns_heating_power_while_heating() {
        let s = ZoneSettings::default();
        let mut z = Zone::new(config("vaskerom", 2));
        let t = ts("2026-11-15T17:00:00Z");
        assert_eq!(
            z.on_power(t, Kw::new(0.03), &s),
            None,
            "idle reports teach nothing"
        );
        assert_eq!(z.heating_kw, None);
        assert_eq!(z.on_power(t, Kw::new(0.5), &s), Some(Kw::new(0.5)));
        let learned = z.on_power(t, Kw::new(1.0), &s).unwrap();
        assert!((learned.value() - 0.6).abs() < 1e-12);
        assert!(z.is_drawing(&s));
    }

    #[test]
    fn hourly_energy_from_the_thermostat_counter() {
        let mut z = Zone::new(config("bad", 1));
        assert_eq!(
            z.on_energy(ts("2026-11-15T17:00:30Z"), Kwh::new(100.0)),
            None
        );
        assert_eq!(
            z.on_energy(ts("2026-11-15T17:30:00Z"), Kwh::new(100.6)),
            None
        );
        assert_eq!(
            z.on_energy(ts("2026-11-15T18:01:00Z"), Kwh::new(101.2)),
            Some((
                HourStart::containing(ts("2026-11-15T17:00:00Z")),
                Kwh::new(101.2) - Kwh::new(100.0)
            ))
        );
        // A report from an earlier hour is ignored.
        assert_eq!(
            z.on_energy(ts("2026-11-15T17:59:00Z"), Kwh::new(101.0)),
            None
        );
        // A gap of an hour or more, or a counter reset, gives nothing.
        assert_eq!(
            z.on_energy(ts("2026-11-15T20:00:00Z"), Kwh::new(103.0)),
            None
        );
        assert_eq!(z.on_energy(ts("2026-11-15T21:00:00Z"), Kwh::new(1.0)), None);
    }

    #[test]
    fn default_settings() {
        let s = ZoneSettings::default();
        assert_eq!(s.echo_tolerance, CelsiusDelta::new(0.25));
        assert_eq!(s.override_duration, SignedDuration::from_hours(2));
        assert_eq!(s.heating_threshold, Kw::new(0.05));
        assert_eq!(s.learn_rate, 0.2);
    }
}

//! `SQLite` persistence behind a `Store` trait, plus an in-memory store for tests.
//!
//! The store keeps what the engine asks it to (`Output::Persist*`) and hands it back as a
//! [`PersistedState`] at startup. Daily peaks are not stored: they are derived from the
//! hourly energy. Timestamps are UTC epoch seconds.
//!
//! [`SqliteStore`] also keeps a sample log for the history graphs: the first status in each
//! minute is recorded (see [`Sample`]), and samples older than the retention are pruned
//! once an hour.

use std::path::Path;

use energybrain_core::energy::{HourEnergy, Quality};
use energybrain_core::engine::{PersistedState, PersistedZone};
use energybrain_core::guard::GuardEvent;
use energybrain_core::guard::GuardStatus;
use energybrain_core::ids::ZoneId;
use energybrain_core::io::{Event, EventKind, Output, Sample, Status, ZoneSample};
use energybrain_core::mode::Mode;
use energybrain_core::time::HourStart;
use energybrain_core::units::{Celsius, Kw, Kwh};
use energybrain_core::zone::Override;
use jiff::{SignedDuration, Timestamp};
use rusqlite::types::FromSql;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

/// Why the store failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// `SQLite` reported an error.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A stored value cannot be understood.
    #[error("corrupt store: {0}")]
    Corrupt(String),
    /// The database was written by a newer version.
    #[error("database schema version {found} is newer than this build knows ({known})")]
    Schema {
        /// Version in the file.
        found: i64,
        /// Newest version this build knows.
        known: i64,
    },
}

/// Persistence for the engine.
pub trait Store: Send {
    /// Loads what the engine needs at startup; hours from `hours_since` on.
    fn load(&self, hours_since: Timestamp) -> Result<PersistedState, StoreError>;
    /// Persists the `Persist*` outputs; other outputs are ignored.
    fn apply(&mut self, outputs: &[Output]) -> Result<(), StoreError>;
}

/// Schema migrations, applied in order. Index + 1 is the schema version.
const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE hourly_energy (
        hour_start_utc INTEGER PRIMARY KEY,
        kwh REAL NOT NULL,
        quality TEXT NOT NULL
    );
    CREATE TABLE zones (
        zone TEXT PRIMARY KEY,
        base REAL,
        last_write REAL,
        last_write_at INTEGER,
        heating_kw REAL,
        shed_allowed INTEGER,
        override_value REAL,
        override_until INTEGER
    );
    CREATE TABLE zone_hourly_energy (
        zone TEXT NOT NULL,
        hour_start_utc INTEGER NOT NULL,
        kwh REAL NOT NULL,
        PRIMARY KEY (zone, hour_start_utc)
    );
    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        at INTEGER NOT NULL,
        kind TEXT NOT NULL,
        zone TEXT,
        json TEXT NOT NULL
    );
    CREATE INDEX events_at ON events (at);
",
    "
    CREATE TABLE samples (
        at INTEGER PRIMARY KEY,
        mode TEXT NOT NULL,
        guard TEXT NOT NULL,
        power_kw REAL,
        used_kwh REAL NOT NULL,
        projected_kwh REAL,
        allowed_kwh REAL NOT NULL,
        limit_kwh REAL NOT NULL
    );
    CREATE TABLE zone_samples (
        at INTEGER NOT NULL,
        zone TEXT NOT NULL,
        idx INTEGER NOT NULL,
        base REAL,
        reported REAL,
        decision REAL,
        power_kw REAL,
        shed INTEGER NOT NULL,
        overridden INTEGER NOT NULL,
        PRIMARY KEY (at, zone)
    ) WITHOUT ROWID;
",
];

/// How long samples are kept unless configured otherwise.
pub const DEFAULT_SAMPLE_RETENTION: SignedDuration = SignedDuration::from_hours(90 * 24);

/// At most one sample per this many seconds.
const SAMPLE_INTERVAL_SECS: i64 = 60;

/// Decides which statuses become samples, and when old ones are pruned.
#[derive(Debug)]
struct Sampler {
    retention: SignedDuration,
    last_minute: Option<i64>,
    last_prune_hour: Option<i64>,
}

impl Sampler {
    fn new() -> Self {
        Self {
            retention: DEFAULT_SAMPLE_RETENTION,
            last_minute: None,
            last_prune_hour: None,
        }
    }

    /// Records `status` if it is the first one in its minute; prunes on the first sample
    /// of each hour.
    fn record(&mut self, tx: &Transaction<'_>, status: &Status) -> Result<(), StoreError> {
        let at = status.at.as_second();
        let minute = at.div_euclid(SAMPLE_INTERVAL_SECS);
        if self.last_minute == Some(minute) {
            return Ok(());
        }
        insert_sample(tx, &Sample::of(status))?;
        self.last_minute = Some(minute);
        let hour = at.div_euclid(3600);
        if self.last_prune_hour != Some(hour) {
            let before = at.saturating_sub(self.retention.as_secs());
            tx.execute("DELETE FROM samples WHERE at < ?1", [before])?;
            tx.execute("DELETE FROM zone_samples WHERE at < ?1", [before])?;
            self.last_prune_hour = Some(hour);
        }
        Ok(())
    }
}

fn insert_sample(tx: &Transaction<'_>, s: &Sample) -> Result<(), StoreError> {
    let at = s.at.as_second();
    tx.execute(
        "INSERT OR REPLACE INTO samples
         (at, mode, guard, power_kw, used_kwh, projected_kwh, allowed_kwh, limit_kwh)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            at,
            s.mode.as_str(),
            s.guard.as_str(),
            s.power.map(Kw::value),
            s.used.value(),
            s.projected.map(Kwh::value),
            s.allowed.value(),
            s.limit.value()
        ],
    )?;
    for (idx, z) in (0_i64..).zip(&s.zones) {
        tx.execute(
            "INSERT OR REPLACE INTO zone_samples
             (at, zone, idx, base, reported, decision, power_kw, shed, overridden)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                at,
                z.id.as_str(),
                idx,
                z.base.map(Celsius::value),
                z.reported.map(Celsius::value),
                z.decision.map(Celsius::value),
                z.power.map(Kw::value),
                z.shed,
                z.overridden
            ],
        )?;
    }
    Ok(())
}

/// The `SQLite` store.
#[derive(Debug)]
pub struct SqliteStore {
    conn: Connection,
    sampler: Sampler,
}

impl SqliteStore {
    /// Opens (creating if needed) and migrates a database file.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    /// An empty in-memory database.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")?;
        let found: i64 = first_value(&conn, "SELECT version FROM schema_version")?.unwrap_or(0);
        let known = i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX);
        if found > known {
            return Err(StoreError::Schema { found, known });
        }
        let tx = conn.transaction()?;
        for sql in MIGRATIONS.iter().skip(usize::try_from(found).unwrap_or(0)) {
            tx.execute_batch(sql)?;
        }
        tx.execute("DELETE FROM schema_version", [])?;
        tx.execute("INSERT INTO schema_version (version) VALUES (?1)", [known])?;
        tx.commit()?;
        Ok(Self {
            conn,
            sampler: Sampler::new(),
        })
    }

    /// Keeps samples for `retention` instead of [`DEFAULT_SAMPLE_RETENTION`].
    #[must_use]
    pub fn with_sample_retention(mut self, retention: SignedDuration) -> Self {
        self.sampler.retention = retention;
        self
    }

    /// The schema version of the open database.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))?)
    }

    /// Stored events (as JSON), newest first, optionally only those at or after `since`.
    pub fn events(
        &self,
        since: Option<Timestamp>,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>, StoreError> {
        let since = since.map_or(i64::MIN, Timestamp::as_second);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self
            .conn
            .prepare("SELECT json FROM events WHERE at >= ?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = stmt.query_map(params![since, limit], read_json)?;
        rows.map(parse_event).collect()
    }

    /// Stored events in `[from, to)` with their time, newest first, at most `limit` (so a cap
    /// drops the oldest).
    pub fn events_between(
        &self,
        from: Timestamp,
        to: Timestamp,
        limit: usize,
    ) -> Result<Vec<(Timestamp, serde_json::Value)>, StoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT at, json FROM events WHERE at >= ?1 AND at < ?2
             ORDER BY at DESC, id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![from.as_second(), to.as_second(), limit],
            read_timed_json,
        )?;
        rows.map(timed_event).collect()
    }

    /// Hourly energy for hours starting in `[from, to)`, oldest first.
    pub fn hours(&self, from: Timestamp, to: Timestamp) -> Result<Vec<HourEnergy>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT hour_start_utc, kwh, quality FROM hourly_energy
             WHERE hour_start_utc >= ?1 AND hour_start_utc < ?2 ORDER BY hour_start_utc",
        )?;
        let rows = stmt.query_map(params![from.as_second(), to.as_second()], read_hour)?;
        rows.map(hour_energy).collect()
    }

    /// Samples taken in `[from, to)`, oldest first, each with its zones in status order.
    pub fn samples(&self, from: Timestamp, to: Timestamp) -> Result<Vec<Sample>, StoreError> {
        let range = params![from.as_second(), to.as_second()];
        let mut stmt = self.conn.prepare(
            "SELECT at, mode, guard, power_kw, used_kwh, projected_kwh, allowed_kwh, limit_kwh
             FROM samples WHERE at >= ?1 AND at < ?2 ORDER BY at",
        )?;
        let rows = stmt.query_map(range, SampleRow::read)?;
        let mut samples = rows
            .map(|row| row?.into_sample())
            .collect::<Result<Vec<_>, _>>()?;
        let mut stmt = self.conn.prepare(
            "SELECT at, zone, base, reported, decision, power_kw, shed, overridden
             FROM zone_samples WHERE at >= ?1 AND at < ?2 ORDER BY at, idx",
        )?;
        for row in stmt.query_map(range, ZoneSampleRow::read)? {
            let (at, zone) = row?.into_zone_sample()?;
            attach(&mut samples, at, zone)?;
        }
        Ok(samples)
    }

    /// Hourly energy per zone (from the thermostats' own counters) for hours starting in
    /// `[from, to)`.
    pub fn zone_hours(
        &self,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<(ZoneId, HourStart, Kwh)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT zone, hour_start_utc, kwh FROM zone_hourly_energy
             WHERE hour_start_utc >= ?1 AND hour_start_utc < ?2 ORDER BY hour_start_utc, zone",
        )?;
        let rows = stmt.query_map(params![from.as_second(), to.as_second()], read_zone_hour)?;
        rows.map(|row| {
            let (zone, hour, kwh) = row?;
            Ok((zone_id(&zone)?, hour_start(hour)?, Kwh::new(kwh)))
        })
        .collect()
    }

    /// Deletes events older than `before`. Returns how many were removed.
    pub fn prune_events(&mut self, before: Timestamp) -> Result<usize, StoreError> {
        Ok(self
            .conn
            .execute("DELETE FROM events WHERE at < ?1", [before.as_second()])?)
    }
}

impl Store for SqliteStore {
    fn load(&self, hours_since: Timestamp) -> Result<PersistedState, StoreError> {
        let mut state = PersistedState::default();

        let mut stmt = self.conn.prepare(
            "SELECT hour_start_utc, kwh, quality FROM hourly_energy WHERE hour_start_utc >= ?1",
        )?;
        let rows = stmt.query_map([hours_since.as_second()], read_hour)?;
        for row in rows {
            let e = hour_energy(row)?;
            state.hours.insert(e.hour, e);
        }

        let mode: Option<String> =
            first_value(&self.conn, "SELECT value FROM settings WHERE key = 'mode'")?;
        state.mode = mode
            .map(|m| Mode::parse(&m).ok_or_else(|| StoreError::Corrupt(format!("mode {m:?}"))))
            .transpose()?;

        let mut stmt = self.conn.prepare(
            "SELECT zone, base, last_write, last_write_at, heating_kw, shed_allowed,
                    override_value, override_until FROM zones",
        )?;
        let rows = stmt.query_map([], ZoneRow::read)?;
        for row in rows {
            let (zone, persisted) = row?.into_persisted()?;
            state.zones.insert(zone, persisted);
        }
        Ok(state)
    }

    fn apply(&mut self, outputs: &[Output]) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        for o in outputs {
            if let Output::PublishStatus(status) = o {
                self.sampler.record(&tx, status)?;
            }
            apply_one(&tx, o)?;
        }
        tx.commit()?;
        Ok(())
    }
}

fn upsert_zone(
    tx: &Transaction<'_>,
    zone: &ZoneId,
    sets: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<(), StoreError> {
    let columns: Vec<&str> = sets.split(',').map(str::trim).collect();
    let placeholders: Vec<String> = (2..2 + columns.len()).map(|i| format!("?{i}")).collect();
    let updates: Vec<String> = columns
        .iter()
        .map(|c| format!("{c} = excluded.{c}"))
        .collect();
    let sql = format!(
        "INSERT INTO zones (zone, {}) VALUES (?1, {}) ON CONFLICT (zone) DO UPDATE SET {}",
        columns.join(", "),
        placeholders.join(", "),
        updates.join(", ")
    );
    let id = zone.as_str();
    let mut all: Vec<&dyn rusqlite::ToSql> = vec![&id];
    all.extend_from_slice(values);
    tx.execute(&sql, all.as_slice())?;
    Ok(())
}

fn apply_one(tx: &Transaction<'_>, o: &Output) -> Result<(), StoreError> {
    match o {
        Output::PersistHour(e) => {
            tx.execute(
                "INSERT INTO hourly_energy (hour_start_utc, kwh, quality) VALUES (?1, ?2, ?3)
                 ON CONFLICT (hour_start_utc) DO UPDATE SET kwh = excluded.kwh, quality = excluded.quality",
                params![e.hour.start().as_second(), e.kwh.value(), quality_str(e.quality)],
            )?;
        }
        Output::PersistZoneWrite { zone, value, at } => {
            upsert_zone(
                tx,
                zone,
                "last_write, last_write_at",
                &[&value.value(), &at.as_second()],
            )?;
        }
        Output::PersistZoneBase { zone, value } => {
            upsert_zone(tx, zone, "base", &[&value.value()])?;
        }
        Output::PersistOverride { zone, manual } => {
            let value = manual.map(|o| o.value.value());
            let until = manual.map(|o| o.until.as_second());
            upsert_zone(
                tx,
                zone,
                "override_value, override_until",
                &[&value, &until],
            )?;
        }
        Output::PersistZoneStats { zone, heating_kw } => {
            upsert_zone(tx, zone, "heating_kw", &[&heating_kw.value()])?;
        }
        Output::PersistShedAllowed { zone, allowed } => {
            upsert_zone(tx, zone, "shed_allowed", &[allowed])?;
        }
        Output::PersistZoneHour { zone, hour, kwh } => {
            tx.execute(
                "INSERT INTO zone_hourly_energy (zone, hour_start_utc, kwh) VALUES (?1, ?2, ?3)
                 ON CONFLICT (zone, hour_start_utc) DO UPDATE SET kwh = excluded.kwh",
                params![zone.as_str(), hour.start().as_second(), kwh.value()],
            )?;
        }
        Output::PersistMode(mode) => {
            tx.execute(
                "INSERT INTO settings (key, value) VALUES ('mode', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [mode.as_str()],
            )?;
        }
        Output::PersistEvent(e) => {
            let (kind, zone) = event_index(e);
            // Serializing plain data structs to a String cannot fail.
            #[allow(clippy::expect_used)]
            let json = serde_json::to_string(e).expect("event serializes");
            tx.execute(
                "INSERT INTO events (at, kind, zone, json) VALUES (?1, ?2, ?3, ?4)",
                params![e.at.as_second(), kind, zone.map(ZoneId::as_str), json],
            )?;
        }
        Output::WriteSetpoint { .. } | Output::WouldWrite { .. } | Output::PublishStatus(_) => {}
    }
    Ok(())
}

/// Event kind name and zone, for indexing.
fn event_index(e: &Event) -> (&'static str, Option<&ZoneId>) {
    match &e.kind {
        EventKind::Guard { event } => match event {
            GuardEvent::Shed { zone, .. } => ("shed", Some(zone)),
            GuardEvent::Release { zone, .. } => ("release", Some(zone)),
            GuardEvent::LimitUnreachable { .. } => ("limit_unreachable", None),
        },
        EventKind::Override { zone, .. } => ("override", Some(zone)),
        EventKind::OverrideExpired { zone } => ("override_expired", Some(zone)),
        EventKind::ModeChanged { .. } => ("mode_changed", None),
    }
}

struct ZoneRow {
    zone: String,
    base: Option<f64>,
    last_write: Option<f64>,
    last_write_at: Option<i64>,
    heating_kw: Option<f64>,
    shed_allowed: Option<bool>,
    override_value: Option<f64>,
    override_until: Option<i64>,
}

impl ZoneRow {
    fn read(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            zone: r.get(0)?,
            base: r.get(1)?,
            last_write: r.get(2)?,
            last_write_at: r.get(3)?,
            heating_kw: r.get(4)?,
            shed_allowed: r.get(5)?,
            override_value: r.get(6)?,
            override_until: r.get(7)?,
        })
    }

    fn into_persisted(self) -> Result<(ZoneId, PersistedZone), StoreError> {
        let last_write = match (self.last_write, self.last_write_at) {
            (Some(v), Some(at)) => Some((Celsius::new(v), timestamp(at)?)),
            _ => None,
        };
        let manual = match (self.override_value, self.override_until) {
            (Some(v), Some(until)) => Some(Override {
                value: Celsius::new(v),
                until: timestamp(until)?,
            }),
            _ => None,
        };
        Ok((
            zone_id(&self.zone)?,
            PersistedZone {
                base: self.base.map(Celsius::new),
                last_write,
                heating_kw: self.heating_kw.map(Kw::new),
                shed_allowed: self.shed_allowed,
                manual,
            },
        ))
    }
}

fn read_hour(r: &Row<'_>) -> rusqlite::Result<(i64, f64, String)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
}

fn hour_energy(row: rusqlite::Result<(i64, f64, String)>) -> Result<HourEnergy, StoreError> {
    let (hour, kwh, quality) = row?;
    Ok(HourEnergy {
        hour: hour_start(hour)?,
        kwh: Kwh::new(kwh),
        quality: parse_quality(&quality)?,
    })
}

fn read_json(r: &Row<'_>) -> rusqlite::Result<String> {
    r.get(0)
}

fn parse_event(json: rusqlite::Result<String>) -> Result<serde_json::Value, StoreError> {
    serde_json::from_str(&json?).map_err(|e| StoreError::Corrupt(format!("event json: {e}")))
}

fn read_timed_json(r: &Row<'_>) -> rusqlite::Result<(i64, String)> {
    Ok((r.get(0)?, r.get(1)?))
}

fn timed_event(
    row: rusqlite::Result<(i64, String)>,
) -> Result<(Timestamp, serde_json::Value), StoreError> {
    let (at, json) = row?;
    Ok((timestamp(at)?, parse_event(Ok(json))?))
}

struct SampleRow {
    at: i64,
    mode: String,
    guard: String,
    power: Option<f64>,
    used: f64,
    projected: Option<f64>,
    allowed: f64,
    limit: f64,
}

impl SampleRow {
    fn read(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            at: r.get(0)?,
            mode: r.get(1)?,
            guard: r.get(2)?,
            power: r.get(3)?,
            used: r.get(4)?,
            projected: r.get(5)?,
            allowed: r.get(6)?,
            limit: r.get(7)?,
        })
    }

    fn into_sample(self) -> Result<Sample, StoreError> {
        let mode = Mode::parse(&self.mode)
            .ok_or_else(|| StoreError::Corrupt(format!("mode {:?}", self.mode)))?;
        let guard = GuardStatus::parse(&self.guard)
            .ok_or_else(|| StoreError::Corrupt(format!("guard {:?}", self.guard)))?;
        Ok(Sample {
            at: timestamp(self.at)?,
            mode,
            guard,
            power: self.power.map(Kw::new),
            used: Kwh::new(self.used),
            projected: self.projected.map(Kwh::new),
            allowed: Kwh::new(self.allowed),
            limit: Kwh::new(self.limit),
            zones: Vec::new(),
        })
    }
}

struct ZoneSampleRow {
    at: i64,
    zone: String,
    base: Option<f64>,
    reported: Option<f64>,
    decision: Option<f64>,
    power: Option<f64>,
    shed: bool,
    overridden: bool,
}

impl ZoneSampleRow {
    fn read(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            at: r.get(0)?,
            zone: r.get(1)?,
            base: r.get(2)?,
            reported: r.get(3)?,
            decision: r.get(4)?,
            power: r.get(5)?,
            shed: r.get(6)?,
            overridden: r.get(7)?,
        })
    }

    fn into_zone_sample(self) -> Result<(i64, ZoneSample), StoreError> {
        Ok((
            self.at,
            ZoneSample {
                id: zone_id(&self.zone)?,
                base: self.base.map(Celsius::new),
                reported: self.reported.map(Celsius::new),
                decision: self.decision.map(Celsius::new),
                power: self.power.map(Kw::new),
                shed: self.shed,
                overridden: self.overridden,
            },
        ))
    }
}

/// Adds a zone's part to the sample taken at `at` (`samples` is sorted by time).
fn attach(samples: &mut [Sample], at: i64, zone: ZoneSample) -> Result<(), StoreError> {
    let i = samples
        .binary_search_by_key(&at, |s| s.at.as_second())
        .map_err(|_| StoreError::Corrupt(format!("zone sample at {at} has no sample")))?;
    samples[i].zones.push(zone);
    Ok(())
}

fn read_zone_hour(r: &Row<'_>) -> rusqlite::Result<(String, i64, f64)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
}

fn timestamp(s: i64) -> Result<Timestamp, StoreError> {
    Timestamp::from_second(s).map_err(|e| StoreError::Corrupt(format!("timestamp {s}: {e}")))
}

fn hour_start(s: i64) -> Result<HourStart, StoreError> {
    Ok(HourStart::containing(timestamp(s)?))
}

/// The first column of the first row, if there is a row. A named function so the caller's `?`
/// shares a line with covered code (see the coverage note in CLAUDE.md).
fn first_value<T: FromSql>(conn: &Connection, sql: &str) -> rusqlite::Result<Option<T>> {
    conn.query_row(sql, [], |r| r.get(0)).optional()
}

fn zone_id(s: &str) -> Result<ZoneId, StoreError> {
    ZoneId::new(s).map_err(|e| StoreError::Corrupt(e.to_string()))
}

fn quality_str(q: Quality) -> &'static str {
    match q {
        Quality::Estimated => "estimated",
        Quality::Interpolated => "interpolated",
        Quality::Measured => "measured",
    }
}

fn parse_quality(s: &str) -> Result<Quality, StoreError> {
    match s {
        "estimated" => Ok(Quality::Estimated),
        "interpolated" => Ok(Quality::Interpolated),
        "measured" => Ok(Quality::Measured),
        other => Err(StoreError::Corrupt(format!("quality {other:?}"))),
    }
}

/// A store that keeps everything in memory (engine and app tests).
#[derive(Debug, Default, Clone)]
pub struct MemoryStore {
    /// Persisted state.
    pub state: PersistedState,
    /// Every event, oldest first.
    pub events: Vec<Event>,
}

impl Store for MemoryStore {
    fn load(&self, hours_since: Timestamp) -> Result<PersistedState, StoreError> {
        let mut state = self.state.clone();
        state.hours.retain(|h, _| h.start() >= hours_since);
        Ok(state)
    }

    fn apply(&mut self, outputs: &[Output]) -> Result<(), StoreError> {
        for o in outputs {
            self.state.apply(o);
            if let Output::PersistEvent(e) = o {
                self.events.push(e.clone());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use energybrain_core::energy::Freshness;
    use energybrain_core::guard::ReleaseReason;
    use energybrain_core::io::ZoneStatus;
    use energybrain_core::peaks::{HourLimit, LimitBasis};
    use energybrain_core::tariff::{Tier, TierIndex};
    use energybrain_core::units::Kr;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn zone(id: &str) -> ZoneId {
        ZoneId::new(id).unwrap()
    }

    fn hour(s: &str) -> HourStart {
        HourStart::containing(ts(s))
    }

    fn event(at: &str, kind: EventKind) -> Output {
        Output::PersistEvent(Event {
            at: ts(at),
            mode: Mode::Observe,
            kind,
        })
    }

    /// One of every state output, some of them twice to exercise upserts.
    fn state_outputs() -> Vec<Output> {
        let e = |h: &str, kwh: f64, quality| {
            Output::PersistHour(HourEnergy {
                hour: hour(h),
                kwh: Kwh::new(kwh),
                quality,
            })
        };
        vec![
            e("2026-10-31T12:00:00Z", 3.0, Quality::Measured),
            e("2026-11-15T17:00:00Z", 4.0, Quality::Estimated),
            e("2026-11-15T17:00:00Z", 4.2, Quality::Measured),
            e("2026-11-15T18:00:00Z", 2.0, Quality::Interpolated),
            Output::PersistZoneBase {
                zone: zone("bathroom"),
                value: Celsius::new(26.5),
            },
            Output::PersistZoneWrite {
                zone: zone("bathroom"),
                value: Celsius::new(22.5),
                at: ts("2026-11-15T17:20:00Z"),
            },
            Output::PersistZoneStats {
                zone: zone("bathroom"),
                heating_kw: Kw::new(1.45),
            },
            Output::PersistShedAllowed {
                zone: zone("guest_1"),
                allowed: false,
            },
            Output::PersistOverride {
                zone: zone("hallway"),
                manual: Some(Override {
                    value: Celsius::new(24.0),
                    until: ts("2026-11-15T19:00:00Z"),
                }),
            },
            Output::PersistOverride {
                zone: zone("basement"),
                manual: Some(Override {
                    value: Celsius::new(18.0),
                    until: ts("2026-11-15T19:00:00Z"),
                }),
            },
            Output::PersistOverride {
                zone: zone("basement"),
                manual: None,
            },
            Output::PersistZoneHour {
                zone: zone("bathroom"),
                hour: hour("2026-11-15T17:00:00Z"),
                kwh: Kwh::new(1.2),
            },
            Output::PersistMode(Mode::Observe),
            Output::PersistMode(Mode::Active),
        ]
    }

    fn event_outputs() -> Vec<Output> {
        vec![
            event(
                "2026-11-15T17:20:00Z",
                EventKind::Guard {
                    event: GuardEvent::Shed {
                        zone: zone("bathroom"),
                        load: Kw::new(1.5),
                        projected: Kwh::new(5.2),
                        limit: Kwh::new(4.7),
                    },
                },
            ),
            event(
                "2026-11-15T17:40:00Z",
                EventKind::Guard {
                    event: GuardEvent::Release {
                        zone: zone("bathroom"),
                        reason: ReleaseReason::BelowLimit,
                    },
                },
            ),
            event(
                "2026-11-15T17:41:00Z",
                EventKind::Guard {
                    event: GuardEvent::LimitUnreachable {
                        projected: Kwh::new(9.0),
                        limit: Kwh::new(4.7),
                        available: Kwh::ZERO,
                    },
                },
            ),
            event(
                "2026-11-15T17:42:00Z",
                EventKind::Override {
                    zone: zone("hallway"),
                    value: Celsius::new(24.0),
                    until: ts("2026-11-15T19:00:00Z"),
                },
            ),
            event(
                "2026-11-15T17:43:00Z",
                EventKind::OverrideExpired {
                    zone: zone("hallway"),
                },
            ),
            event(
                "2026-11-15T17:44:00Z",
                EventKind::ModeChanged {
                    from: Mode::Observe,
                    to: Mode::Active,
                },
            ),
        ]
    }

    /// One of every output.
    fn outputs() -> Vec<Output> {
        let mut all = state_outputs();
        all.extend(event_outputs());
        all.extend([
            Output::WriteSetpoint {
                zone: zone("bathroom"),
                value: Celsius::new(22.5),
                reasons: vec![],
            },
            Output::WouldWrite {
                zone: zone("bathroom"),
                value: Celsius::new(22.5),
                reasons: vec![],
            },
        ]);
        all
    }

    /// What the engine itself would hold after the same outputs.
    fn expected(since: Timestamp) -> PersistedState {
        let mut p = PersistedState::default();
        for o in outputs() {
            p.apply(&o);
        }
        p.hours.retain(|h, _| h.start() >= since);
        p
    }

    const SINCE: &str = "2026-11-01T00:00:00Z";

    #[test]
    fn sqlite_round_trip_matches_the_engines_own_view() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        assert_eq!(s.load(ts(SINCE)).unwrap(), PersistedState::default());
        s.apply(&outputs()).unwrap();
        let loaded = s.load(ts(SINCE)).unwrap();
        assert_eq!(loaded, expected(ts(SINCE)));
        assert_eq!(
            loaded.hours.len(),
            2,
            "the October hour is outside the window"
        );
        assert_eq!(loaded.zones[&zone("basement")].manual, None);
    }

    #[test]
    fn memory_store_matches_sqlite() {
        let mut m = MemoryStore::default();
        m.apply(&outputs()).unwrap();
        assert_eq!(m.load(ts(SINCE)).unwrap(), expected(ts(SINCE)));
        assert_eq!(m.events.len(), 6);
    }

    #[test]
    fn events_are_queryable_newest_first() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        s.apply(&outputs()).unwrap();
        let all = s.events(None, 100).unwrap();
        assert_eq!(all.len(), 6);
        assert_eq!(all[0]["kind"]["type"], "mode_changed");
        assert_eq!(all[5]["kind"]["event"]["type"], "shed");
        let recent = s.events(Some(ts("2026-11-15T17:42:00Z")), 2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[1]["kind"]["type"], "override_expired");
        let kinds: Vec<String> = s
            .conn
            .prepare("SELECT kind || ':' || coalesce(zone, '-') FROM events ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            kinds,
            [
                "shed:bathroom",
                "release:bathroom",
                "limit_unreachable:-",
                "override:hallway",
                "override_expired:hallway",
                "mode_changed:-"
            ]
        );
        assert_eq!(s.prune_events(ts("2026-11-15T17:42:00Z")).unwrap(), 3);
        assert_eq!(s.events(None, 100).unwrap().len(), 3);
    }

    #[test]
    fn zone_hours_are_queryable() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        s.apply(&outputs()).unwrap();
        assert_eq!(
            s.zone_hours(ts(SINCE), ts("2026-11-16T00:00:00Z")).unwrap(),
            [(
                zone("bathroom"),
                hour("2026-11-15T17:00:00Z"),
                Kwh::new(1.2)
            )]
        );
        assert_eq!(
            s.zone_hours(ts(SINCE), ts("2026-11-15T17:00:00Z")).unwrap(),
            []
        );
    }

    /// A status at `at` with one fully known zone and one that has reported nothing.
    fn status(at: &str) -> Status {
        let at = ts(at);
        let z = |id: &str, name: &str| ZoneStatus {
            id: zone(id),
            name: name.to_owned(),
            base: None,
            reported: None,
            decision: None,
            reasons: vec![],
            shed: false,
            shed_allowed: true,
            override_until: None,
            power: None,
            heating_kw: None,
        };
        Status {
            at,
            mode: Mode::Observe,
            guard: GuardStatus::Shedding,
            hour: HourStart::containing(at),
            used: Kwh::new(1.5),
            projected: Some(Kwh::new(5.2)),
            power: Some(Kw::new(6.1)),
            freshness: Freshness::Fresh,
            meter_last_seen: Some(at),
            limit: HourLimit {
                allowed: Kwh::new(5.0),
                effective: Kwh::new(4.7),
                basis: LimitBasis::TierBound,
            },
            top_days: vec![],
            top_average: Kw::ZERO,
            tier: Tier {
                index: TierIndex(1),
                from: Kw::new(2.0),
                to: Some(Kw::new(5.0)),
                price: Kr::new(233.0),
            },
            zones: vec![
                ZoneStatus {
                    base: Some(Celsius::new(26.5)),
                    reported: Some(Celsius::new(22.5)),
                    decision: Some(Celsius::new(22.5)),
                    shed: true,
                    override_until: Some(at),
                    power: Some(Kw::new(1.4)),
                    ..z("bathroom", "Bathroom")
                },
                z("basement", "Basement"),
            ],
            charger: None,
        }
    }

    fn publish(s: &mut SqliteStore, statuses: &[Status]) {
        let out: Vec<Output> = statuses
            .iter()
            .map(|st| Output::PublishStatus(Box::new(st.clone())))
            .collect();
        s.apply(&out).unwrap();
    }

    fn sample_times(s: &SqliteStore) -> Vec<String> {
        s.samples(Timestamp::MIN, Timestamp::MAX)
            .unwrap()
            .iter()
            .map(|x| x.at.to_string())
            .collect()
    }

    #[test]
    fn the_first_status_of_each_minute_becomes_a_sample() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        let first = status("2026-11-15T17:20:05Z");
        let mut later = status("2026-11-15T17:21:00Z");
        later.mode = Mode::Active;
        later.guard = GuardStatus::Idle;
        later.power = None;
        later.projected = None;
        later.zones.reverse();
        publish(
            &mut s,
            &[first.clone(), status("2026-11-15T17:20:55Z"), later.clone()],
        );
        let samples = s.samples(Timestamp::MIN, Timestamp::MAX).unwrap();
        assert_eq!(samples, [Sample::of(&first), Sample::of(&later)]);
        assert_eq!(
            samples[1].zones[0].id,
            zone("basement"),
            "status order kept"
        );
        let range = s
            .samples(ts("2026-11-15T17:21:00Z"), ts("2026-11-15T17:22:00Z"))
            .unwrap();
        assert_eq!(range, [Sample::of(&later)]);
        // Other outputs leave the log alone; the memory store ignores statuses.
        s.apply(&outputs()).unwrap();
        assert_eq!(sample_times(&s).len(), 2);
        let mut m = MemoryStore::default();
        m.apply(&[Output::PublishStatus(Box::new(first))]).unwrap();
        assert_eq!(m.state, PersistedState::default());
    }

    #[test]
    fn samples_older_than_the_retention_are_pruned_once_an_hour() {
        let mut s = SqliteStore::open_in_memory()
            .unwrap()
            .with_sample_retention(SignedDuration::from_hours(24));
        publish(
            &mut s,
            &[
                status("2026-11-14T10:00:00Z"),
                status("2026-11-14T10:30:00Z"),
            ],
        );
        // 24 h later, in a new hour: only what is older than a day goes.
        publish(&mut s, &[status("2026-11-15T10:15:00Z")]);
        assert_eq!(
            sample_times(&s),
            ["2026-11-14T10:30:00Z", "2026-11-15T10:15:00Z"]
        );
        // Same hour: no pruning, even though 10:30 yesterday is now more than a day old.
        publish(&mut s, &[status("2026-11-15T10:45:00Z")]);
        assert_eq!(sample_times(&s).len(), 3);
        publish(&mut s, &[status("2026-11-15T11:00:00Z")]);
        assert_eq!(
            sample_times(&s),
            [
                "2026-11-15T10:15:00Z",
                "2026-11-15T10:45:00Z",
                "2026-11-15T11:00:00Z"
            ]
        );
        let zones: i64 = s
            .conn
            .query_row("SELECT count(*) FROM zone_samples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(zones, 6, "zone parts are pruned with their samples");
    }

    #[test]
    fn hours_and_events_are_queryable_by_range() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        s.apply(&outputs()).unwrap();
        let hours = s
            .hours(ts("2026-11-15T17:00:00Z"), ts("2026-11-15T19:00:00Z"))
            .unwrap();
        assert_eq!(
            hours
                .iter()
                .map(|h| (h.hour, h.kwh, h.quality))
                .collect::<Vec<_>>(),
            [
                (
                    hour("2026-11-15T17:00:00Z"),
                    Kwh::new(4.2),
                    Quality::Measured
                ),
                (
                    hour("2026-11-15T18:00:00Z"),
                    Kwh::new(2.0),
                    Quality::Interpolated
                )
            ]
        );
        assert_eq!(
            s.hours(ts("2026-11-15T18:00:00Z"), ts("2026-11-15T18:00:00Z"))
                .unwrap(),
            []
        );
        let events = s
            .events_between(ts("2026-11-15T17:40:00Z"), ts("2026-11-15T17:43:00Z"), 100)
            .unwrap();
        let kinds: Vec<&str> = events
            .iter()
            .map(|(_, e)| e["kind"]["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["override", "guard", "guard"],
            "newest first, end excluded"
        );
        for (at, e) in &events {
            assert_eq!(e["at"], at.to_string(), "the time is the event's own");
        }
        let (at, newest) = &s.events_between(Timestamp::MIN, Timestamp::MAX, 1).unwrap()[0];
        assert_eq!(newest["at"], at.to_string(), "a cap keeps the newest");
        assert!(*at > ts("2026-11-15T17:43:00Z"));
    }

    #[test]
    fn file_database_survives_reopening_and_migrates_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("eb.db");
        {
            let mut s = SqliteStore::open(&path).unwrap();
            assert_eq!(s.schema_version().unwrap(), 2);
            s.apply(&outputs()).unwrap();
        }
        let s = SqliteStore::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), 2);
        assert_eq!(s.load(ts(SINCE)).unwrap(), expected(ts(SINCE)));
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.db");
        drop(SqliteStore::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE schema_version SET version = 99", [])
            .unwrap();
        drop(conn);
        let err = SqliteStore::open(&path).unwrap_err();
        assert_eq!(
            err.to_string(),
            "database schema version 99 is newer than this build knows (2)"
        );
    }

    #[test]
    fn every_quality_round_trips() {
        for q in [Quality::Estimated, Quality::Interpolated, Quality::Measured] {
            assert_eq!(parse_quality(quality_str(q)).unwrap(), q);
        }
    }

    #[test]
    fn corrupt_rows_are_reported() {
        let corrupt = |sql: &str| {
            let s = SqliteStore::open_in_memory().unwrap();
            s.conn.execute_batch(sql).unwrap();
            s.load(ts(SINCE)).unwrap_err().to_string()
        };
        assert_eq!(
            corrupt("INSERT INTO hourly_energy VALUES (1794848400, 1.0, 'guessed')"),
            "corrupt store: quality \"guessed\""
        );
        assert_eq!(
            corrupt("INSERT INTO settings VALUES ('mode', 'turbo')"),
            "corrupt store: mode \"turbo\""
        );
        assert!(
            corrupt("INSERT INTO zones (zone) VALUES ('Bad Zone')")
                .starts_with("corrupt store: zone id")
        );
        assert!(corrupt("INSERT INTO zones (zone, last_write, last_write_at) VALUES ('bathroom', 1, 9223372036854775807)")
            .starts_with("corrupt store: timestamp"));
        assert!(
            corrupt("INSERT INTO hourly_energy VALUES (9223372036854775807, 1.0, 'measured')")
                .starts_with("corrupt store: timestamp")
        );
        assert!(corrupt("INSERT INTO zones (zone, override_value, override_until) VALUES ('bathroom', 1, 9223372036854775807)")
            .starts_with("corrupt store: timestamp"));
        assert!(
            corrupt("INSERT INTO hourly_energy VALUES (1794848400, 'x', 'measured')")
                .starts_with("sqlite: ")
        );

        let s = SqliteStore::open_in_memory().unwrap();
        s.conn
            .execute_batch("INSERT INTO events (at, kind, json) VALUES (0, 'x', 'not json')")
            .unwrap();
        assert!(
            s.events(None, 1)
                .unwrap_err()
                .to_string()
                .starts_with("corrupt store: event json")
        );
        s.conn
            .execute_batch("INSERT INTO zone_hourly_energy VALUES ('Bad Zone', 1794848400, 1.0)")
            .unwrap();
        assert!(s.zone_hours(Timestamp::MIN, Timestamp::MAX).is_err());
        s.conn
            .execute_batch("INSERT INTO hourly_energy VALUES (1794848400, 1.0, 'guessed')")
            .unwrap();
        assert!(s.hours(Timestamp::MIN, Timestamp::MAX).is_err());

        let corrupt_sample = |sql: &str| {
            let s = SqliteStore::open_in_memory().unwrap();
            s.conn.execute_batch(sql).unwrap();
            s.samples(Timestamp::MIN, Timestamp::MAX)
                .unwrap_err()
                .to_string()
        };
        let sample = |mode: &str, guard: &str| {
            format!(
                "INSERT INTO samples VALUES (1794848400, '{mode}', '{guard}', NULL, 1, NULL, 5, 4.7);"
            )
        };
        assert_eq!(
            corrupt_sample(&sample("turbo", "idle")),
            "corrupt store: mode \"turbo\""
        );
        assert_eq!(
            corrupt_sample(&sample("observe", "busy")),
            "corrupt store: guard \"busy\""
        );
        assert!(corrupt_sample(&format!(
            "{} INSERT INTO zone_samples VALUES (1794848400, 'Bad Zone', 0, NULL, NULL, NULL, NULL, 0, 0)",
            sample("observe", "idle")
        ))
        .starts_with("corrupt store: zone id"));
        assert_eq!(
            corrupt_sample(&format!(
                "{} INSERT INTO zone_samples VALUES (1794848460, 'bathroom', 0, NULL, NULL, NULL, NULL, 0, 0)",
                sample("observe", "idle")
            )),
            "corrupt store: zone sample at 1794848460 has no sample"
        );
        assert!(corrupt_sample(&format!(
            "{} INSERT INTO zone_samples VALUES (1794848400, 'bathroom', 0, 'x', NULL, NULL, NULL, 0, 0)",
            sample("observe", "idle")
        ))
        .starts_with("sqlite: "));
        assert!(corrupt_sample(
            "INSERT INTO samples VALUES (1794848400, 'observe', 'idle', NULL, 'x', NULL, 5, 4.7)"
        )
        .starts_with("sqlite: "));
    }

    #[test]
    fn failing_sqlite_calls_surface_as_errors() {
        // A directory cannot be opened as a database.
        let dir = tempfile::tempdir().unwrap();
        assert!(
            SqliteStore::open(dir.path())
                .unwrap_err()
                .to_string()
                .starts_with("sqlite: ")
        );
        // Only the zones table missing: load fails at that step.
        let s = SqliteStore::open_in_memory().unwrap();
        s.conn.execute_batch("DROP TABLE zones").unwrap();
        assert!(s.load(ts(SINCE)).unwrap_err().to_string().contains("zones"));
        // A dropped table makes every statement fail.
        let mut s = SqliteStore::open_in_memory().unwrap();
        s.conn.execute_batch("DROP TABLE zones; DROP TABLE events; DROP TABLE zone_hourly_energy; DROP TABLE hourly_energy; DROP TABLE settings").unwrap();
        for o in outputs() {
            let ignored = matches!(
                o,
                Output::WriteSetpoint { .. } | Output::WouldWrite { .. } | Output::PublishStatus(_)
            );
            assert_eq!(s.apply(&[o]).is_err(), !ignored);
        }
        assert!(s.load(ts(SINCE)).is_err());
        assert!(s.events(None, 1).is_err());
        assert!(s.zone_hours(Timestamp::MIN, Timestamp::MAX).is_err());
        assert!(s.hours(Timestamp::MIN, Timestamp::MAX).is_err());
        assert!(s.events_between(Timestamp::MIN, Timestamp::MAX, 1).is_err());
        assert!(s.prune_events(ts(SINCE)).is_err());
        s.conn.execute_batch("DROP TABLE schema_version").unwrap();
        assert!(s.schema_version().is_err());
    }

    #[test]
    fn failing_sample_writes_and_reads_surface_as_errors() {
        let failing = |drop: &str, st: Status| {
            let mut s = SqliteStore::open_in_memory().unwrap();
            s.conn.execute_batch(drop).unwrap();
            let write = s.apply(&[Output::PublishStatus(Box::new(st))]).is_err();
            let read = s.samples(Timestamp::MIN, Timestamp::MAX).is_err();
            (write, read)
        };
        let st = status("2026-11-15T17:20:00Z");
        assert_eq!(failing("DROP TABLE samples", st.clone()), (true, true));
        assert_eq!(failing("DROP TABLE zone_samples", st.clone()), (true, true));
        // Without zones the insert succeeds and pruning trips over the missing table.
        let no_zones = Status {
            zones: vec![],
            ..st
        };
        assert_eq!(failing("DROP TABLE zone_samples", no_zones), (true, true));
    }
}

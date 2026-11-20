# energybrain — implementation plan (phase 1: peak guard)

This plan is the hand-over from an earlier design discussion. Read it fully before writing
code, then work through the milestones in order. Each milestone must end with
`just check` green (fmt, clippy, tests, **100 % line coverage**).

---

## 1. What we are building

`energybrain` is a long-running service (Rust, shipped as a Docker container) that
controls electric floor heating in a Norwegian home to keep grid-rent (nettleie) costs
down. It talks to everything over MQTT and is exposed to Apple Home via the existing
Homebridge **Easy MQTT** plugin. No Homebridge plugin is written.

### Context
- Electricity is on **Norgespris** (fixed energy price), so spot-price optimisation is
  not the goal. What varies is grid rent:
  - **Capacity charge (kapasitetsledd):** monthly tier chosen from the **average of the
    three highest hourly consumptions (kWh/h) in the month, on three different days**.
  - **Energy charge (energiledd):** cheaper at night (22–06) and weekends/holidays.
    Not used in phase 1, but the tariff model must be able to hold it later.
- Z-Wave runs through **Z-Wave JS UI**, which publishes to MQTT in *value only* mode with
  topics under `zwave/<room>/<device>/...`. Setpoints are written by publishing to the
  value topic with a `/set` suffix.
- Whole-house consumption comes from the smart meter's **HAN port** via a local reader
  that publishes to MQTT (exact model/format to be confirmed — see M0).

### Phase 1 goal: the peak guard
Keep each hour's whole-house consumption below a limit that keeps the month in the
target capacity tier. Do it by temporarily lowering floor-heating setpoints zone by zone
(in priority order), and restoring them afterwards. Ship with an **observe-only mode**
that computes and reports everything but never writes setpoints, so it can run for a
few weeks before going live.

### Non-goals for phase 1 (but the architecture must make them easy later)
- Schedules / comfort profiles, presence/away mode
- Night/weekend tariff shifting, Norwegian public holidays
- Weather compensation (met.no Locationforecast + outdoor sensor)
- Heat-pump coordination
- Monthly 5 000 kWh Norgespris cap and spot-price fallback
- Model-based optimiser (LP via `good_lp` + `microlp`)
- A metrics stack (user runs none). Observe mode is reviewed through the built-in history
  page (§8), `/api/events` and `/api/state`; `/metrics` is kept so Grafana can be added
  later

---

## 2. Architecture

**Functional core, imperative shell.** All decisions live in a pure, synchronous,
I/O-free core driven by an explicit clock. Adapters translate the outside world into
typed `Input`s and typed `Output`s back into side effects.

```
 HAN reader ─┐                                          ┌─► zwave/.../set   (setpoints)
 Thermostats ┼─► MQTT ─► [mqtt adapter] ─► Input ─┐     ├─► energybrain/*    (HomeKit status)
 Home app ───┘   (Easy MQTT command topics)        │     │
                                                   ▼     │
                                   ┌──────── Engine (pure) ───────┐
                                   │ state: meter, hours, peaks,  │
                                   │        zones, mode, overrides│
                                   │ policies → proposals         │
                                   │ arbiter  → zone decisions    │
                                   │ actuator → diffed commands   │
                                   └──────────────┬───────────────┘
                                                  │ Output
                       ┌──────────────────────────┼───────────────────────┐
                       ▼                          ▼                       ▼
                 SQLite store              mqtt adapter            /metrics, /api/state,
            (hourly kWh, overrides,                                /api/day, /api/month,
             last written setpoints,                               / (history page, §8)
             1-minute samples)                                     (axum)
```

### Cargo workspace layout

```
energybrain/                     (repo root = the folder Claude Code starts in)
├── Cargo.toml                   workspace, shared lints, profile settings
├── justfile                     check / test / cov / run / docker
├── CLAUDE.md
├── PLAN.md
├── config/
│   └── example.toml             fully commented example config
├── crates/
│   ├── core/      energybrain-core    pure domain: units, clock, time, tariff, energy
│   │                                  accounting, peaks, zones, policies, arbiter,
│   │                                  actuator, engine. NO tokio, NO I/O.
│   ├── config/    energybrain-config  TOML → validated typed config (serde)
│   ├── mqtt/      energybrain-mqtt    rumqttc adapter: topic mapping, payload parsing,
│   │                                  Input/Output translation, Easy MQTT topics
│   ├── han/       energybrain-han     pure Kaifa HDLC/DLMS frame parser (Tibber Pulse raw)
│   ├── store/     energybrain-store   rusqlite (bundled) persistence behind a trait
│   ├── http/      energybrain-http    axum: /healthz, /metrics, /api/*, history page
│   ├── app/       energybrain         binary: runtime loop (lib.rs) + main.rs plumbing
│   └── sim/       energybrain-sim     dev only: simulated house (meter, thermostats,
│                                      loads) + `energybrain-sim` binary, see §7
├── tests/fixtures/              recorded/synthetic HAN + thermostat traces
├── deploy/
│   ├── Dockerfile
│   ├── docker-compose.yml       energybrain only (metrics stack dropped for phase 1)
│   └── dev/mosquitto/           mosquitto.conf for the local dev broker (§7)
└── .github/workflows/ci.yml
```

Rule: `energybrain-core` depends on nothing async and nothing that does I/O. Adapters
depend on core; core never depends on adapters. `app` is the only crate that knows
about all the others.

### Core concepts (energybrain-core)

**Units (newtypes, no raw f64 across module boundaries):**
`Kw`, `Kwh`, `Celsius`, `CelsiusDelta`, `Kr` (for tier prices). Implement only the
arithmetic that makes physical sense (e.g. `Kw * Duration -> Kwh`, `Kwh / Duration -> Kw`,
`Celsius - Celsius -> CelsiusDelta`). No `Kw + Kwh`.

**Clock:** `trait Clock { fn now(&self) -> jiff::Timestamp; }` with `SystemClock`
(in app) and `SimClock` (in core, test-support feature). The engine never reads time on
its own — `now` is passed into every call.

**Time:** all stored timestamps are UTC (`jiff::Timestamp`). A `LocalCalendar`
(timezone from config, default `Europe/Oslo`) answers: hour-start for a timestamp,
local date for an hour, hours in a day. Must be correct across DST (23- and 25-hour
days). Hour keys are UTC hour starts; "day" for the top-3 rule is the **local** date.

**Tariff:** `CapacityTariff { tiers: Vec<Tier { from: Kw, to: Option<Kw>, price: Kr }> }`
with `tier_for(avg: Kw) -> TierIndex` (bands are half-open `[from, to)`). The struct has room for `EnergyTariff`
(day/night/weekend windows) later; do not implement it now beyond the type stub if it
costs coverage — prefer leaving it out entirely.

**Inputs** (enum, exhaustive):
- `MeterCumulative { at, total: Kwh }` — cumulative import register (usually hourly)
- `MeterPower { at, power: Kw }` — instant import power (every few seconds)
- `ThermostatSetpoint { zone, at, value: Celsius }` — reported setpoint
- `ZonePower { zone, at, power: Kw }` — optional, if the thermostat reports power
- `ZoneEnergy { zone, at, total: Kwh }` — optional, the thermostat's own kWh counter
- `SetMode(Mode)` — `Off | Observe | Active`
- `SetShedAllowed { zone, allowed: bool }`
- `Tick { at }`

**Outputs** (enum):
- `WriteSetpoint { zone, value: Celsius }`
- `PublishStatus(Status)` — mode, guard active, per-zone shed state, projection, limit
- `PersistHour { hour_start, kwh }`
- `PersistZoneWrite { zone, value, at }`
- `PersistOverride { zone, until: Option<Timestamp> }`
- `PersistZoneBase { zone, value }` — the learned user setpoint (echo-safe restarts)
- `PersistZoneHour { zone, hour_start, kwh }` and `PersistZoneStats { zone, heating_kw }`
  (learned zone load, see Zones)
- `PersistEvent(Event)` — guard decisions (shed / would shed / release / override /
  limit unreachable) with reasons, for reviewing observe mode without a metrics stack

**Energy accounting (`energy` module):**
- Hour kWh comes primarily from deltas of the cumulative register at hour boundaries
  (this matches billing). If the boundary reading is late, attribute by linear
  interpolation between the two surrounding readings.
- Within the current hour, `used_so_far` is the last cumulative reading's delta plus
  the integral of `MeterPower` since that reading (trapezoidal).
- If no cumulative reading exists at all yet, fall back to integrating power only, and
  mark the hour `Estimated`.
- `projected = used_so_far + current_power × remaining_fraction_of_hour`, where
  `current_power` is a short moving average (configurable window, default 60 s) to
  avoid reacting to a kettle spike.
- Data freshness: if no `MeterPower` for `stale_after` (default 120 s), the meter is
  `Stale`.

**Monthly peaks (`peaks` module):**
- From completed hours of the current local month, compute each local day's max hour,
  and the top 3 days.
- `hour_limit(ledger, calendar, hour, target, margin) -> HourLimit` (decided with the user;
  deliberately simple):
  - `allowed` = the target tier's upper bound (kWh/h), raised to today's highest
    completed hour if that is higher (only each day's max counts, so that is free);
  - `effective = allowed − safety_margin` (config, default 0.3 kWh) is what the guard
    acts on;
  - no month-end relaxation and no "tier already lost" mode: always aim for the target
    tier, since lower use never costs more;
  - month rollover (local time) resets the ledger.
- Top-3 average and current tier are still computed for status.

**Zones:** `Zone { id, name, setpoint_topic, power_topic, energy_topic, nominal_kw: Kw
(optional, default 1 kW), priority: u8, shed_delta: CelsiusDelta, min_setpoint,
max_setpoint, shed_allowed }`. Priority, `shed_allowed` and `shed_delta` are config;
`shed_allowed` can also be toggled at runtime from HomeKit (persisted, wins over config).

**Zone learning (user request: "monitor the usage constantly and learn"):** each zone
learns `heating_kw`, an exponential moving average of its power reports while heating
(above 50 W), persisted across restarts. Its kWh counter gives per-zone hourly energy,
persisted for review. Phase 1 uses `heating_kw` only as the load estimate before a
zone's first power report after a restart (replacing `nominal_kw` once learned); later
phases can use the per-zone history for prediction.

**Policies:** a policy looks at a read-only `Context { now, calendar, meter, month,
zones, mode, overrides }` and returns proposals.

```rust
pub trait Policy {
    fn name(&self) -> PolicyName;
    fn propose(&self, ctx: &Context<'_>) -> Vec<Proposal>;
}

pub struct Proposal { pub zone: ZoneId, pub kind: ProposalKind, pub reason: Reason }

pub enum ProposalKind {
    Base(Celsius),               // what the zone "wants" absent other constraints
    Adjust(CelsiusDelta),        // future: weather, tariff shifting
    Cap(Celsius),                // upper limit (peak guard)
    Override(Celsius),           // human wins
}
```

Phase 1 policies:
1. **`UserSetpointBase`** — the base for each zone is the user's own setpoint, learned
   from `ThermostatSetpoint` reports that are *not* echoes of our own writes (an echo =
   within ±0.25 °C of our last write, regardless of elapsed time, until a different value
   is reported; the learned base is persisted so a restart mid-shed never adopts the shed
   value as base). This makes
   phase 1 non-invasive: heating stays "always on" at whatever the user set, and the
   guard only dips it temporarily. A future `SchedulePolicy` replaces this.
2. **`PeakGuard`** — (implementation note: the shedding *decision* is stateful and lives in
   `guard::GuardState::update`, owned by the engine; the `PeakGuardCaps` policy only turns
   the current shed set into `Cap` proposals, so `Policy::propose` stays `&self`.)
   Only zones currently drawing power (> `heating_threshold`) are candidates — shedding an
   idle floor saves nothing now. The projection is reduced by a **credit** for shed zones
   the meter cannot show: a zone still drawing (always true in observe mode, where nothing
   is written) is credited its whole saving since the shed, `load × (hour end − shed time)`;
   a zone that really switched off is credited `load × remaining` for `settle` (default
   90 s) while the moving average catches up. Without it
   the guard would shed every zone in observe mode. A zone is released only if the
   projection *with its load back* stays below `limit − release_margin`.
   If `projected > effective_limit`, choose zones to shed in ascending
   priority (lowest priority number sheds first — document the convention), skipping
   zones with `shed_allowed = false` or an active override, until the expected reduction
   `Σ zone_load × remaining_fraction` covers the excess. `zone_load` = last reported
   `ZonePower` (Z-Wave reports on change, so it is a last-known value, not freshness-gated;
   a zone whose relay is off contributes 0), else learned `heating_kw`, else `nominal_kw`.
   Emits `Cap(base − shed_delta)` (per-zone `shed_delta`, default 4 °C, clamped to
   `min_setpoint`).
   - **Hysteresis:** release a zone only when `projected < effective_limit − release_margin`
     and the zone has been shed for at least `min_shed_duration`.
   - **Hour rollover:** no forced release. The new hour's budget starts fresh and the
     normal hysteresis decides. All releases are **staggered** (`restore_stagger`, e.g.
     2 min apart in reverse shed order) so all floors don't start at once.
   - **Unreachable limit:** if shedding every allowed zone still cannot bring the
     projection under the limit (e.g. EV charging), shed them all anyway and report
     `LimitUnreachable` in status and events, so observe mode shows whether the target
     tier is realistic.
   - **Stale meter:** guard proposes nothing (all zones restore) and status reports
     `MeterStale`.
3. **`ManualOverride`** — if the user raises a setpoint above the cap while the zone is
   shed (a non-echo report), treat it as an override for `override_duration` (default
   2 h): propose `Override(value)` and exclude the zone from shedding until it expires.
   A non-echo report at or below the cap just becomes the new base (no override). When an
   override expires its value remains the base. Overrides persist across restarts.

**Arbiter:** pure function `arbitrate(zone, proposals) -> Decision { value, reasons }`:
`Base` (required; if absent → no decision, don't touch the zone) → sum of `Adjust` →
min of all `Cap` → `Override` replaces everything → clamp to `[min_setpoint,
max_setpoint]`. Every decision carries the list of contributing reasons, and they go out
in status, `/api/state` and `tracing` logs.

**Actuator:** turns decisions into `WriteSetpoint` outputs only when needed:
deadband (default 0.25 °C), per-zone minimum interval between writes (default 60 s,
except restores, which are never delayed beyond the stagger), and never in `Observe` or
`Off` mode. In `Observe`, it still computes "would write" and reports it in
status/metrics. **Leaving `Active`** (to `Observe` or `Off`) first writes every zone we
changed back to its base, as the last act of `Active`, then switches; after that nothing
is written (user decision). In `Off` the guard is disabled entirely.

**Engine:** `Engine::new(config, persisted_state, now)`,
`Engine::handle(&mut self, input: Input) -> Vec<Output>`. `Tick` drives evaluation
(default every 10 s). The engine is the single owner of state, with no interior
mutability or locks.

**Startup safety:** on start the engine loads persisted state. Any zone whose last
written value differs from the base it would choose now gets restored (unless the guard
legitimately wants it shed this hour). Default mode on first ever start: `Observe`.

### Adapters

**energybrain-config:** TOML file path from `ENERGYBRAIN_CONFIG` env/CLI arg. Validates:
unique zone ids, tiers sorted and contiguous, margins non-negative, setpoint bounds
sane, topics non-empty. Errors are typed and point at the offending key.

**energybrain-mqtt** (`rumqttc`, async):
- Topic → Input mapping is fully config-driven (no hardcoded device topics). HAN payloads
  are raw Kaifa HDLC/DLMS frames from a Tibber Pulse and are decoded by
  `energybrain-han` (list 1/2 → `MeterPower`, hourly list 3 → `MeterCumulative`, using
  the meter's own frame timestamp). Thermostat payloads are plain numbers (Z-Wave JS UI
  "value only").
- Output → publish: setpoint writes to `<setpoint_topic>/set` with a plain number
  payload; status topics retained.
- Easy MQTT control/status topics (prefix configurable, default `energybrain`):
  - `energybrain/mode` (retained state) + `energybrain/mode/set` — `off|observe|active`.
    For HomeKit, also `energybrain/active` + `/set` as an on/off switch meaning
    `active` vs `observe`.
  - `energybrain/peakguard/active` — `1/0` (contact sensor)
  - `energybrain/zone/<id>/shed_allowed` + `/set` — `1/0` (switch)
  - `energybrain/zone/<id>/shed` — `1/0` (state)
  - `energybrain/status` — JSON snapshot (for debugging and maybe later use)
- Reconnect is rumqttc's event loop. The adapter resubscribes on `ConnAck` and
  republishes retained status.
- The adapter is split into a **pure translation layer** (topic/payload ↔ Input/Output;
  unit-tested exhaustively) and a **thin transport layer** (rumqttc). The transport is
  covered by integration tests against an **embedded `rumqttd` broker started in-process
  on a random port** (dev-dependency). Decided in M7: rumqttd works (no Docker needed).

**energybrain-store** (`rusqlite` with the `bundled` feature):
- `trait Store { load() -> PersistedState; apply(&[Output]) }`, plus an in-memory
  implementation for engine tests.
- Tables: `hourly_energy(hour_start_utc INTEGER PK, kwh REAL, quality TEXT)`,
  `zones(zone TEXT PK, base, last_write, last_write_at, heating_kw, shed_allowed,
  override_value, override_until)` (all nullable), `settings(key TEXT PK, value TEXT)`
  (mode, shed_allowed per zone), `zone_hourly_energy(zone TEXT, hour_start_utc INTEGER,
  kwh REAL, PK(zone, hour_start_utc))`, `zone_stats(zone TEXT PK, heating_kw REAL)`,
  `events(id INTEGER PK, at INTEGER, kind TEXT, zone TEXT NULL, detail TEXT)` (`SqliteStore::prune_events` exists but is not scheduled in phase 1: a few
  dozen events a day is negligible). Migration 2 adds `samples` and `zone_samples`
  (1-minute status snapshots for the history page, pruned after a retention period, §8).
  Schema migrations run on open, with a
  `schema_version` table.
- Daily peaks are **derived** from `hourly_energy`, not stored separately.
- Tested against in-memory and tempfile databases.
- The async app calls it from a dedicated blocking task (`spawn_blocking` or a
  channel-fed thread).

**energybrain-http** (`axum`):
- `GET /healthz` — 200 when the engine loop is alive, MQTT is connected and the meter
  isn't stale; 503 with a JSON reason otherwise.
- `GET /metrics` — Prometheus text, rendered directly from the status (see decision log).
- `GET /api/state` — JSON: mode, current hour, used_so_far, projection, limit, top-3
  days, tier, per-zone base/decision/reasons/shed state, learned heating power, overrides.
  Quantities serialize as `{"value": …, "unit": "kWh"}` (see decision log).
- `GET /api/events?since=<rfc3339>&limit=<n>` — persisted guard events (shed, would-shed,
  release, override, limit unreachable) with reasons, newest first.
- Tested with `tower::ServiceExt::oneshot`, no real socket needed.

**Metrics (names fixed now so the dashboard can be built against them):**
`energybrain_hour_used_kwh`, `energybrain_hour_projected_kwh`, `energybrain_hour_limit_kwh`,
`energybrain_meter_power_kw`, `energybrain_month_top_kwh{rank="1|2|3"}`,
`energybrain_month_avg_top3_kwh`, `energybrain_tier`, `energybrain_zone_setpoint_celsius{zone,kind="base|decision|reported"}`,
`energybrain_zone_shed{zone}`, `energybrain_zone_would_write_total{zone}` (observe mode),
`energybrain_zone_writes_total{zone}`, `energybrain_meter_last_seen_seconds`,
`energybrain_mqtt_connected`, `energybrain_mode{mode}`.

### App (energybrain binary)
- `lib.rs`: `run(config, clock, mqtt, store, shutdown) -> Result<()>`: a `tokio::select!`
  loop over MQTT inputs, tick interval, HTTP state requests (via `watch` channel of
  the latest state snapshot), and shutdown. Fully tested with fakes plus one
  end-to-end test (embedded broker + tempfile SQLite + SimClock-driven ticks).
- `main.rs`: **only** plumbing: parse args/env, init `tracing-subscriber` (JSON logs
  when `ENERGYBRAIN_LOG_FORMAT=json`), build real adapters, install the SIGTERM/SIGINT
  handler, call `run`. It is the only file excluded from coverage, so keep it under
  ~60 lines with no branches worth testing.

---

## 3. Crates (pin at latest compatible versions when adding)

| Purpose | Crate |
|---|---|
| Runtime | `tokio` (rt-multi-thread, macros, time, signal, sync) |
| MQTT | `rumqttc` (dev: `rumqttd`) |
| Time/TZ | `jiff` |
| Serde | `serde`, `serde_json`, `toml` |
| SQLite | `rusqlite` (`bundled`) |
| HTTP | `axum`, `tower` (dev, `util`) |
| HTTPS client (Easee) | `reqwest` (`rustls`, `json`; §9) |
| Metrics | `metrics`, `metrics-exporter-prometheus` |
| Logging | `tracing`, `tracing-subscriber` |
| Errors | `thiserror` (libraries); `anyhow` only in `main.rs` if at all |
| Testing | `proptest`, `insta` (yaml/json), `tempfile`, `pretty_assertions` |
| Coverage | `cargo-llvm-cov` (tool, not dependency) |

Avoid adding anything else without noting why in CLAUDE.md.

---

## 4. Testing & coverage policy

- **Gate:** `cargo llvm-cov --workspace --all-features --fail-under-lines 100
  --ignore-filename-regex '(^|/)main\.rs$'`. Wired into `just cov` and CI.
- No coverage-exclusion attributes or `cfg(coverage)` tricks. If a line can't be tested,
  refactor it (inject the dependency, split pure from effectful), don't hide it.
- Prefer making invalid states unrepresentable over defensive branches that can never
  run (those are uncoverable by definition).
- **Kinds of tests:**
  - Unit tests next to the code for every module.
  - **Property tests** (`proptest`):
    - the arbiter output is always within `[min, max]`;
    - a zone with `shed_allowed=false` or an active override is never capped;
    - if no earlier day broke the target, ending the hour at the effective limit keeps the
      top-3 average in the target tier;
    - hour accounting sums equal the total cumulative delta;
    - DST: every UTC hour maps to exactly one local date, and local days have 23/24/25
      hours.
  - **Scenario/replay tests:** a closed-loop simulation in `core/src/scenarios.rs` (house
    load, meter every 2 s plus hourly register, thermostats that echo writes and switch off
    when shed) driving `Engine`. (Changed from CSV traces; see the decision log.)
    Assert outputs and snapshot the decision log with `insta`. Required scenarios:
    1. quiet hour → no action
    2. evening spike → sheds lowest-priority zone first, then more, restores with
       hysteresis
    3. spike in the last 5 min of the hour vs the first 5 min (different reductions
       needed)
    4. kettle blip shorter than the averaging window → no action
    5. stale meter mid-shed → all restore
    6. user raises setpoint while shed → override, zone excluded for duration
    7. hour rollover → staggered restore
    8. observe mode → zero `WriteSetpoint`, non-zero "would write"
    9. today already has an hour above the bound → limit relaxed to it for the rest of
       the day; an earlier *day* above the bound does not relax anything
    10. month rollover
    11. DST forward (March) and back (October) days
    12. restart mid-shed → correct restore/continue from persisted state
    13. leaving Active restores every shed zone at once
    14. EV charging → limit unreachable, reported once
  - **Adapter integration tests:** embedded broker for MQTT, tempfile SQLite,
    `oneshot` for HTTP.
- `just check` = `cargo fmt --check && cargo clippy --workspace --all-targets
  --all-features -- -D warnings && cargo test --workspace --all-features && just cov`.

---

## 5. Milestones

Each milestone: write tests alongside the code, finish with `just check` green, commit
with a descriptive message. Don't start the next milestone with a red gate.

**M0 — Discovery (ask the user before writing adapter code).** Ask for, and record the
answers in `config/example.toml` comments and CLAUDE.md:
1. HAN reader model, its MQTT topics, and a sample payload for both instant power and
   cumulative import (and their units).
2. For each floor-heating zone: thermostat model, Z-Wave JS UI setpoint value topic, a
   sample payload, whether it reports power (and that topic), nominal heater kW, and
   desired shed priority.
3. Grid company and its capacity tier table (kW bands and kr/month).
4. Target tier, MQTT broker host/port/auth, and where the container will run.
M1–M6 need none of this, so they can be done in parallel with waiting for answers.

**M1 — Skeleton & tooling.** Workspace, crates with empty `lib.rs`, shared
`[workspace.lints]` (`unsafe_code = "forbid"`, clippy pedantic as warn with a short
allow-list), `justfile`, `rust-toolchain.toml` (pinned, see decision log), CI workflow
(fmt, clippy, test, coverage gate), and the `.gitignore`. Initialise git.

**M2 — Core foundations.** Units, `Clock`/`SimClock`, `LocalCalendar` with DST tests,
`CapacityTariff`, ids, error types.

**M3 — Energy accounting.** Meter state, hourly kWh from cumulative deltas plus
interpolation, power integration, moving average, projection, staleness.

**M4 — Monthly peaks & limit.** Top-3 distinct-day computation, `hour_limit`, month
rollover. This is the heart of the guard, so property-test it heavily.

**M5 — Zones, policies, arbiter, actuator.** `UserSetpointBase` (with echo detection),
`PeakGuard` (selection, hysteresis, stagger, stale handling), `ManualOverride`,
`arbitrate`, actuator diffing and modes.

**M6 — Engine & scenario tests.** `Engine::handle`, persisted-state load/apply,
startup safety, and all 12 replay scenarios with fixtures and snapshots.

**M7 — Config + MQTT adapter.** TOML schema and `config/example.toml`, the translation
layer, the rumqttc transport, the embedded-broker integration tests, and the Easy MQTT
topics.

**M8 — Store + HTTP.** SQLite store and migrations, `/healthz`, `/metrics`,
`/api/state`.

**M9 — App wiring & deployment.** `run()` loop, `main.rs`, the end-to-end test, a
multi-stage `Dockerfile` (build on `rust:<stable>`, run on `gcr.io/distroless/cc` or
static musl on `scratch`, non-root, `/data` volume for SQLite, `/config` for TOML),
`docker-compose.yml` with energybrain only (amd64). VictoriaMetrics + Grafana are dropped
for phase 1 (user decision); the metric names stay fixed so they can be added later.

**M10 — Docs & go-live checklist.** README covering what it does, configuration
reference, Easy MQTT accessory config snippets for each topic above, how to run in
observe mode, and how to read the dashboard. Go-live checklist: 2+ weeks in observe
mode with no false sheds, verify "would write" events against real evenings, then
switch the HomeKit switch to active.

---

## 6. Extending later (design must keep these one-module changes)

| Future feature | How it plugs in |
|---|---|
| Schedules / comfort profiles (idea A) | New `SchedulePolicy` emitting `Base`, replacing `UserSetpointBase` per zone via config |
| Presence / away | New `Input::Presence`, a policy emitting `Adjust` (negative) or `Base` |
| Night/weekend tariff shifting (C) | `EnergyTariff` + Norwegian holidays in core, `TariffShiftPolicy` emitting `Adjust` (pre-heat at night, coast in the day) |
| Weather compensation (E) | New adapter crate `energybrain-metno` (reqwest) + outdoor sensor topic → `Input::Forecast`/`Input::OutdoorTemp`, `WeatherPolicy` emitting `Adjust` |
| Heat pump coordination (B) | New zone kind/actuator for the heat pump driver over MQTT, plus a policy that lowers floor base when the heat pump is carrying the load |
| 5 000 kWh Norgespris cap | Monthly kWh already derivable from `hourly_energy`. Add a spot-price adapter and a policy activated above the cap |
| EV charger (Easee Charge Lite) | Planned in §9: adapter crate `energybrain-easee` for Easee's cloud API, a controllable load with a current cap; `PeakGuard` sheds it before floors |
| Optimiser (F) | `energybrain-planner` crate using `good_lp` + `microlp`, producing a per-hour plan the `PlanPolicy` follows, still subject to `PeakGuard` caps |
| Custom UI | Extend `energybrain-http` with endpoints and static assets (as the history page, §8, does) |

Adding an `Input` variant or `ProposalKind` variant should cause compile errors at every
place that must handle it. Don't use `_ =>` catch-alls on these enums.

---

## 7. Local development: simulated house

Goal: run energybrain on the dev Mac, away from the home network, against a simulated house
that publishes live data on the same topics and in the same formats as the real devices,
with time running faster (default 6×: one hour in ten minutes).

**Purpose: a developer tool for watching behaviour, not a test harness.** The point is to
tweak values (energybrain's config, the house, live control topics) and see for yourself
what energybrain does. The guard's correctness is verified by the core scenario tests and
the app's end-to-end test, not by the simulator. Its own tests exist for the 100 % line
coverage gate and to keep its wire format in step with energybrain's parser, and no more.

### Decisions
- **Production keeps mosquitto.** Embedding a broker (rmqtt/rumqttd) in energybrain would
  make every release or crash take down the house's MQTT bus (Z-Wave JS UI, Homebridge,
  Tibber Pulse), and would put broker code under the coverage gate. Rejected.
- **Dev broker: mosquitto in a container**: Apple `container` (CLI 1.0) on macOS, Docker
  on Linux, overridable with `EB_RUNTIME`. It's the same broker as production, needs no broker code, and is started by
  `just dev`. It runs anonymous and **without persistence**, and `just dev` recreates it
  every run, so retained topics from an earlier run (mode, setpoints, other simulated
  times) never leak into the next one.
- **The simulated house and energybrain run natively in one process** (`energybrain-sim`
  binary). Both need the same accelerated clock, and one shared `Arc<dyn Clock>` is far
  simpler than keeping two processes in agreement. Running natively also keeps
  edit → `cargo run` fast.
- **The simulated house is an ordinary MQTT client.** It publishes real Kaifa HDLC frames
  and Z-Wave JS UI value-only numbers, so the real translator, the HAN parser and the
  transport are all exercised. No dev-only code paths in energybrain's adapters.
- **Fresh state every run.** Simulated time runs ahead of the wall clock, so a reused
  database would already hold hours "after" the next run's start. `just dev` deletes the
  dev database (`target/dev/energybrain.db`) before starting.
- **No new dependencies.** The randomness in the house model comes from a hand-rolled,
  seeded SplitMix64 (a few lines) instead of `rand`, so runs are reproducible from
  `seed`. The sim uses `rumqttc`, `tokio`, `jiff`, `serde` and `toml`, which are already
  in the workspace; `rumqttd` stays a dev-dependency (sim tests).

### Accelerated time
- `energybrain_core::clock::ScaledClock::new(inner: Arc<dyn Clock>, start, factor:
  NonZeroU16)`: at construction it records `real_anchor = inner.now()`, then
  `now() = start + (inner.now() − real_anchor) × factor` (saturating). It is pure, because
  it only reads its inner clock, so it belongs in core and is tested with `SimClock` as the
  inner clock. `ScaledClock::real(simulated)` converts a simulated duration into the real
  one, e.g. for the tick.
- Every engine duration (settle, min shed, stagger, staleness, override, actuator
  min-interval) is measured with `clock.now()`, so it scales automatically.
- Real-time timers in `app` that must scale: the `tick` (10 s → `10 s / factor`), which
  `Runtime` already takes as a parameter. `retry_delay`, MQTT keep-alive and the 2 s
  shutdown flush stay in real time, because they concern the real broker.
  `/healthz`'s `max_status_age = max(30 s, 3 × tick)` is compared against simulated
  time; with the tick scaled down it stays 30 s simulated, which is correct (one status
  per 10 s simulated).
- `factor` is limited to 1–3600. Above ~60× the real MQTT round trip becomes noticeable
  in simulated time (100 ms real = 6 s simulated at 60×). That's fine for the
  guard, but worth knowing when reading events.

### Configuration
Two files, so the production config schema (unknown keys rejected) stays free of dev keys:
- `config/dev.toml`: a normal energybrain config, the same as `example.toml` except
  `mqtt.host = "127.0.0.1"`, no credentials, `mqtt.port = 1884`, `http.listen = "0.0.0.0:8082"` (read-only, reachable from the LAN) (off the usual ports, so dev runs next to a real mosquitto/energybrain),
  `store.path = "target/dev/energybrain.db"`. Committed (it holds no secrets).
- `config/sim.toml`: parsed by the sim crate, with the same validation style as the
  energybrain config (typed errors naming the key, unknown keys rejected):
  ```toml
  factor = 6                       # simulated seconds per real second (1–3600)
  start = "2026-12-01T16:00"       # required; local time in the energybrain config's zone
  seed = 1                         # every random choice derives from it
  initial_mode = "active"          # written to the fresh dev database before start (optional)
  outdoor_c = -5.0                 # heat-pump load and floor heat loss
  indoor_c = 20.0                  # floors lose heat to indoor + 20 % towards outdoor
  register_kwh = 10000             # the meter's A+ register at the start

  [background]                     # whole-house load excluding floors, EV, heat pump
  profile_kw = [0.35, 0.3, …]      # 24 values, one per local hour (default: a typical day)
  noise_kw = 0.15                  # uniform ± deviation, resampled every simulated minute

  [heat_pump]
  max_kw = 1.5                     # linear from idle at 15 °C to full at −15 °C outdoors

  [[appliance]]                    # random short loads: kettle, oven, washer, …
  name = "kettle"; kw = 2.0; minutes = 3; per_day = 4; from_hour = 6; to_hour = 23

  [[ev]]                           # daily sessions, plus manual control (below)
  from = "17:30"; kwh = 20.0; kw = 7.4

  [[floor]]                        # per energybrain zone id; physics only, all optional
  zone = "bathroom"; heater_kw = 1.5; setpoint = 26.0
  capacity_kwh_per_k = 0.5; loss_kw_per_k = 0.08; ambient_c = 12.0
  ```
  The simulated thermostats are built from the energybrain config's zones and topics, so
  they always match what energybrain subscribes to. `sim.toml` only adds the physical
  model per zone id. A zone it doesn't list gets defaults (heater = the zone's
  `nominal_kw`, setpoint 22 °C); an unknown or repeated zone id is an error.

### Simulated devices (what goes on the wire)
- **Meter** (topic `meter.topic`, raw bytes), timed in simulated time:
  - list 1 (power, W) every 2.5 s, list 2 every 10 s;
  - list 3 at hh:00:10, with the meter clock set to hh:00:10 and the A+ register value
    as it was at hh:00:00, matching the real meter and `Register::captured_at`;
  - the header date-time is local wall time with deviation `0x8000` and the DST bit
    `0x80` set in summer;
  - one Pulse-style `{"status":{…}}` JSON message now and then, which must be ignored.
- **Thermostats** (Z-TRM3 per zone, value-only plain numbers), timed in simulated time:
  - setpoint, published retained at startup and on change;
  - on `<setpoint>/set`, the new value is echoed twice within 1 s;
  - power in W (~0.1 when idle), on change and every ~90 s;
  - the kWh counter, which grows with the heater's energy, published every 5 min.
- **Floor model:** one thermal mass per floor,
  `capacity · dT/dt = heater − loss · (T − environment)`, where the environment is
  `ambient_c` if set, else `indoor + 0.2 · (outdoor − indoor)`. The thermostat has ±0.5 °C
  hysteresis around its setpoint, and clamps writes to 5–40 °C and rounds them to its 0.5 °C
  step, as a Z-TRM3 does. A shed (setpoint − 4 °C) therefore keeps the heater off
  until the floor really cools, as in reality. A user change via MQTT (publishing a new
  setpoint on the device's own topic, as the Z-Wave panel would) is supported, to test
  overrides.
- **Whole-house power** = background profile + noise + appliances + heat pump + EV +
  Σ floor heaters. It is integrated into the A+ register.
- **Reproducible randomness:** noise, appliance starts and EV plug-ins are only decided
  on whole simulated-minute ticks, so a run depends on `seed` (and on control messages),
  not on how finely the driver steps. An EV `from` time that falls in the skipped hour of
  spring-forward doesn't start that day; one in the repeated autumn hour starts twice.
- **Discrete events:** `House::step(to)` runs every due event (frames, reports, echoes,
  minute ticks, the hour-shift capture) in time order and integrates the physics between
  them. If a run starts exactly on the hour, its first list 3 at hh:00:10 carries the
  starting register, as a real meter's would.
- **Control topics** (`sim/…`, for poking at a live run from MQTT Explorer or
  `mosquitto_pub`):
  - `sim/ev/set` takes `off` or a power in kW;
  - `sim/load/set` adds extra load in kW;
  - `sim/meter/set` takes `pause` or `resume`, to test the stale-meter path;
  - `sim/outdoor/set` takes a temperature in °C;
  - `sim/zone/<id>/setpoint/set` is a user setpoint change, as if made on the panel;
  - `sim/state` is retained JSON with simulated time, the load breakdown and floor
    temperatures, republished every simulated minute.

### Structure (same functional-core pattern as energybrain)
- `crates/han`: add `encode` (list 1/2/3 frame builder, HDLC framing with FCS, COSEM
  date-time). Tests: re-encoding the parsed values of each real fixture in
  `tests/fixtures/han/` reproduces it byte for byte; a proptest checks that
  `parse(encode(x)) == x`.
- `crates/sim` (`energybrain-sim`):
  - `config.rs`: `sim.toml` → typed `SimConfig`, validated against the energybrain
    `Config`;
  - `rng.rs`: SplitMix64;
  - `house.rs`: the pure model. `House::step(now) -> Vec<Publish>` and
    `House::on_message(topic, payload, now) -> Vec<Publish>`. No I/O and no clock: time
    comes in as an argument. It gets most of the tests, including insta snapshots of a
    simulated hour;
  - `driver.rs`: the thin async loop. It uses energybrain's own MQTT transport
    (`energybrain_mqtt::transport`, plus a `publish_bytes` for HAN frames), steps the
    house on a real-time interval with `now` from the shared clock, hands it inbound
    messages, and logs a one-line summary each simulated minute (time, total power by
    load, heating floors) next to energybrain's own log lines. Bad control payloads are
    logged, not fatal. Tested against an embedded `rumqttd` for coverage: messages
    arrive, a `/set` is echoed, a bad control is survived, a disconnect is logged,
    shutdown returns;
  - `main.rs`: plumbing only, excluded from coverage by the existing `main.rs` regex.
    It loads both configs, builds the `ScaledClock` over `SystemClock`, seeds the fresh
    store with `initial_mode` (`Output::PersistMode`), spawns the driver and runs
    `energybrain::run` with tick `clock.real(10 s)`, then stops both on Ctrl-C.
- No end-to-end test of the simulator together with energybrain: it would mostly repeat
  the scenario tests, on timing that is hard to make deterministic. `just dev` is where
  the two meet.

### just recipes
- `just dev`, which:
  1. with Apple `container`, runs `container system start` (does nothing if already running);
  2. removes any old `energybrain-mqtt` container;
  3. runs `<runtime> run -d --name energybrain-mqtt -p 127.0.0.1:1884:1883`, with
     `deploy/dev/mosquitto` mounted as `/mosquitto/config`, using `eclipse-mosquitto:2`;
  4. removes `target/dev/energybrain.db*`;
  5. runs `cargo run -p energybrain-sim -- config/dev.toml config/sim.toml`.

  energybrain's MQTT connection already retries, so a broker that is still starting is
  harmless.
- `just dev-down`: stops and removes the broker container.
- `just sim <control> <value>` publishes `sim/<control>/set`, `just dev-pub <topic> <value>`
  anything, and `just dev-sub [topic]` watches the broker, all through `<runtime> exec` into the
  broker container (no local mosquitto clients needed).
- To verify in S5: whether Apple `container` can mount a single file. The plan mounts
  the directory to avoid depending on that.

### Milestones
Each ends with `just check` green and a commit, as for M0–M10.

**S1 — HAN encoder.** `energybrain_han::encode` + fixture round-trip + proptest.

**S2 — ScaledClock.** In core, with tests: anchor, scaling, factor 1 is identity, and a
DST-day start.

**S3 — House model.** `sim` crate: `SimConfig` parsing/validation, SplitMix64, floor
physics, the meter and thermostat publishing schedules, echo on `/set`, control topics.
Pure and fully unit-tested, with a snapshot of one simulated hour.

**S4 — Driver + dev binary.** MQTT driver (tests against embedded `rumqttd`), `main.rs`,
`config/dev.toml`, `config/sim.toml`.

**S5 — Recipes + docs.** `deploy/dev/mosquitto/mosquitto.conf`, `just dev` /
`just dev-down`, `docs/dev.md` (how to run, control topics, reading `/api/state` and
`/api/events` in simulated time), a README link, and CLAUDE.md (commands, decision log:
the new crate, mosquitto in Apple containers for dev, no embedded broker in production).

---

## 8. History page (graphs instead of Grafana)

Goal: see how usage and the guard's decisions played out over a day or a month, without
running a metrics stack. `/api/state` only shows *now*, and the store only keeps hourly
totals and events, so the minute-by-minute picture (power, the hour budget, setpoints,
sheds) is lost. Grafana would have got it from VictoriaMetrics scraping `/metrics`.

### Decisions
- **Served by energybrain itself**, at `GET /`: one static HTML page, a small JS file and
  [uPlot](https://github.com/leeoniya/uPlot) 1.6.32 (MIT, ~50 KB), all embedded in the
  binary with `include_str!` and vendored in `crates/http/assets/` (with its licence), so
  the page works without internet access and needs no new Rust dependency.
- **Rust does the work, JS only draws.** Day boundaries (DST days have 23 or 25 hours),
  per-hour limits, day peaks, the top 3 and the tier are computed in Rust and tested
  under the coverage gate. The JS fetches JSON, maps it to uPlot series and handles
  navigation; it has no logic worth testing. (SVG rendered in Rust was considered: fully
  tested, but slower to build and without zoom/hover. The JSON endpoints stay the same
  if the page is ever replaced.)
- **Sample log.** The store records a sample of the status at most once per minute
  (the first `PublishStatus` in each minute, in the app clock's time, so `just dev`
  fills it in accelerated time): power, used, projection, limit (allowed and
  effective), mode, guard state, and per zone base / reported / decision setpoint, power,
  shed and override. Samples older than `store.sample_retention_days` (default 90, at
  least 1) are pruned once an hour. ~0.5 MB/day for six zones.
- Hourly energy, zone hourly energy and events are still **kept forever**: after the
  retention period a day still shows its hourly bars and every decision, just not the
  minute curves.
- Sampling lives in the store (it handles `PublishStatus` in `apply`), not in core: a
  snapshot for graphs is not a decision, and core stays unchanged apart from the plain
  `Sample` data type (`Sample::of(&Status)`).
- The floors' total power per sample (`floors`) is also added up in Rust, so the page has
  nothing to sum.
- **Columnar JSON** for samples (`{"unit": "kW", "values": [...]}` per series, plus one
  `at` array of RFC 3339 timestamps), which is what uPlot wants and ~5× smaller than one
  object per sample with a `{value, unit}` per number.
- Times in the page are shown in the configured time zone (uPlot's `tzDate`), not the
  browser's.

### Endpoints
- `GET /api/day?date=YYYY-MM-DD` (default: today, local): `date`, `time_zone`, `from`,
  `to`, `samples` (columnar, per zone too), `hours` (each local hour: energy, quality,
  and the hour's allowed/effective limit from its last sample), `zone_hours`, `event_hours`
  (the events grouped by local hour, newest hour and newest event first, hours without
  events left out; each with its index into `hours`, start/end and whether its energy went
  over the limit).
- `GET /api/month?month=YYYY-MM` (default: this month): per local day its peak hour and
  energy, its total, and its hourly values (for a day × hour heatmap); the top 3 days,
  their average, the resulting tier and the target.
- Bad dates are 400 with a JSON error, store failures 500, as for `/api/events`.

### Page
- Header: mode, guard state and the running hour (used / projected / limit), from
  `/api/state`, refreshed every minute while today is shown.
- **Day** (‹ date ›): hourly energy bars against the allowed limit (estimated hours
  marked); house power with the floors' power stacked under it; the hour budget (used,
  projection, effective limit); one small chart per zone with base, reported and decided
  setpoint and shed periods shaded; the event list, newest first, under one header per
  hour (energy / limit, event count, flagged when over the limit; clicking it zooms to
  the hour). Charts share the cursor; dragging, or
  clicking an hour bar, zooms every minute chart to that range.
- **Month** (‹ month ›): day-peak bars with the top 3 highlighted, the target and the
  top-3 average as lines, the tier and its price, and a day × hour heatmap.

### Milestones
Each ends with `just check` green and a commit.

**H1 — Sample log.** `Sample` in core, store migration 2 (`samples`, `zone_samples`),
sampling and pruning in `SqliteStore::apply`, range queries for samples, hours, zone
hours and events; `store.sample_retention_days` in the config.

**H2 — Day and month APIs.** `/api/day`, `/api/month` in `energybrain-http`, built from a
`HistoryQuery` that the app backs with SQLite; insta snapshots of both, including a DST
day.

**H3 — Page.** Vendored uPlot, `GET /` and `/assets/*`, the page, docs (`docs/docker.md`,
`docs/dev.md`) and the CLAUDE.md decision log.


---

## 9. EV charger (Easee Charge Lite)

Goal: make the EV charger the guard's first and biggest lever. The car (Jaguar I-Pace)
charges on **1 phase** on the IT net: 6 A ≈ 1.4 kW, 10 A ≈ 2.3 kW, 15 A ≈ 3.5 kW, 32 A ≈
7.4 kW. It is usually run at 10–15 A from the Easee app, which on its own is most of the
4.7 kWh/h budget. No "full by 07:00" target is needed yet (user decision): the charger may
be shed freely. Easee's and Tibber's smart charging are off, so nothing else controls it.

### Easee API (developer.easee.com, read 2026-11-19)
Only the cloud API exists (no local interface on the Charge Lite). Base URL
`https://api.easee.com`; `chargerId` is the charger's serial number.
- **Auth:** `POST /api/accounts/login` `{userName, password}` → `{accessToken, expiresIn
  (s), refreshToken, tokenType: "Bearer"}`. `POST /api/accounts/refresh_token
  {refreshToken}` returns a new pair. A refresh token is **single use** (always keep the
  newest) and can be revoked when its session is idle; on failure, log in again with the
  password. Lifetimes are not documented: use `expiresIn`.
- **Read:** `GET /state/{serial}/observations?ids=…` (note `/state`, not `/api`), **100
  requests per 5 min**. It returns `{observations: [{id, timestamp, dataType, value}]}`.
  The ids we use are 109 op mode, 120 total power (kW), 121 session energy (kWh), 48 dynamic
  charger current (A, volatile), 47 max charger current (A, non-volatile), 114 output
  current (A, what the pilot signal offers the car), 96 reason for no current, and 250
  connected to cloud. The old `GET /api/chargers/{id}/state` is no longer in the spec, so
  we don't use it.
- **Op mode (109):** 0 Offline, 1 Disconnected, 2 AwaitingStart, 3 Charging,
  4 Completed, 5 Error, 6 ReadyToCharge, 7 AwaitingAuthentication, 8 Deauthenticating.
- **Write:** `POST /api/chargers/{id}/commands/set_dynamic_charger_current {amps,
  minutes}`. It sets a volatile limit for `minutes` (a TTL, after which it is cleared;
  0 = until replaced). The limit is also cleared on reboot or when the cable is unplugged.
  The response is a `Command` with `status` 0 Sent / 1 Expired / 2 Executed / 3 Rejected.
  No rate limit is stated for this endpoint.
- **Not used:**
  - the `settings` endpoint (20/min; `dynamicChargerCurrent` there is deprecated, and
    Easee says non-dynamic values must not be changed by automation, to spare the flash);
  - circuit-level dynamic current (only meaningful on a circuit master);
  - `pause_charging` / `resume_charging` (see Decisions);
  - SignalR streaming (no good Rust client, and the HAN meter already gives real-time
    power).
- Easee: "The lowest current value in the charger is the one the charger will use"; below
  6 A the car can't charge; "some cars might get upset if the current is changed too
  frequently".
- Throttling (429, `Retry-After`) is not documented.

### Decisions
- **Control only through the TTL'd dynamic current.** Every limit is written with
  `minutes = ttl` (default 10) and refreshed after `ttl / 2` while it still applies. If
  energybrain dies or loses internet, the limit expires and the charger goes back to the
  app's own setting. A dead-man switch costs one write per 5 min, and refreshing the same
  value doesn't change the pilot signal. The app has two controls, confirmed by the user
  on 2026-11-20: "Charger limit" (in the menu) is the max current (observation 47), and
  "Temporary charging limit" (on the main screen) is the dynamic current (48).
  "Charger limit" is the user's ceiling (lowest wins).
- **Release = write `max_current` (32 A) with a short TTL (1 min).** We can't delete a
  limit, but 32 A is above anything the app allows, so "lowest wins" leaves the app's
  value in charge. A release is a write, so it obeys the same rules as zones: never in
  `Observe`/`Off`, except that leaving `Active` and shutdown release a charger we limited.
- **Pause = `amps = 0` through the same command**, not `pause_charging`. It expires by
  itself like every other limit, and `resume_charging` isn't needed. Pausing is off by
  default (`pause_allowed = false`) until the I-Pace has been seen to resume reliably
  (E0). Without it, 6 A is the floor.
- **Shed order:** charger first, step by step down to `min_current` (6 A), then pause if
  allowed, then floors by priority. This matches the "sheddable before floors" in §6.
  Raising works like a zone release: only when the projection *with the extra amps back*
  stays below `limit − release_margin`, and never more than `max_raise_step` (default 4 A)
  at a time.
- **Hysteresis for the car:**
  - whole amps only;
  - at most one change per `min_change_interval` (default 2 min), with no bypass (dropped
    in E2: a change may jump straight to the needed level, and while it waits the charger's
    possible saving still counts against the excess, so floors aren't shed for it);
  - a pause is held at least `min_pause` (default 15 min);
  - at most `max_pauses_per_hour` (default 2). After that, 6 A is the floor for the rest of
    the hour.
- **Load model:** charger power per amp is a configured voltage, 230 V × `phases`
  (`ChargerConfig::voltage`), not learned (changed in E1). Learning `total_power /
  output_current` would be wrong whenever the car draws less than it is offered, and
  230 V vs a measured ~225 V doesn't change any decision. `Ampere × Volt → Kw` and
  `Kw / Volt → Ampere` are the new unit arithmetic. The guard's
  expected saving for a step from `a` to `b` amps is `(a − b) × V × remaining`, capped at
  the charger's last reported power, since the car may draw less than offered.
- **The meter stays the truth for the projection.** Charger readings are 30 s polls used
  only for the load estimate, the status breakdown (house without EV) and echo checks.
  They are stale after `stale_after` (default 3 min, or `Offline`/not connected to the
  cloud). While they are stale the charger is not a candidate (no writes, no credit) and
  the guard falls back to floors.
- **Echo and confirmation:** a write is confirmed when observation 48 reports the written
  value. It counts as current for `min_change_interval`, like `awaiting_echo` on zones.
  After that it is presumed lost and retried. A `Rejected`/`Expired` command status is
  logged as an event.
- **Temporary limit set in the app = override.** The app's "Temporary charging limit"
  writes the same dynamic current we do. If observation 48 reports a value we didn't
  write, the user has set it: the charger is excluded from shedding for
  `override_minutes` (default 120), or until the cable is unplugged, and the guard uses
  floors only. This mirrors a thermostat override. Without it, the user setting 6 A would
  be overwritten by our 10 A, or their 15 A by our 6 A. A reboot or unplug clears 48 by
  itself, and that is not an override. How a cleared 48 reads wasn't checked (user skipped
  it), so the rule only counts a value as the user's when it is **below 47**, isn't
  something we wrote, and isn't 0. "No limit", whether it reads as absent, 32 or equal to
  47, is never mistaken for a user limit. A user pause (0 A) is the one case that isn't
  recognised. The adapter logs the raw value of 48 at debug level, so observe mode shows
  how a cleared limit really reads.
- **Observe mode** credits the charger saving it *would* have made, exactly as for zones
  (the whole saving since the decision, banked on release). Otherwise observe mode would
  go on to shed every floor while the car charges. Events `WouldLimitCharger { amps,
  reasons }` show how often the car would have been adjusted or paused, which is the input
  for deciding `pause_allowed` and the intervals.
- **HomeKit:** `energybrain/ev/shed_allowed` + `/set` (switch, persisted, wins over
  config) and `energybrain/ev/limited` (state), like zones.
- **New dependency:** `reqwest` (`default-features = false`, `rustls-tls`, `json`) for the
  HTTPS client, recorded in the CLAUDE.md decision log when added. This is the first TLS in
  the project. Tests use a fake Easee built on `axum` (already a dependency) on a free
  port, so no mocking crate is needed.
- **Throttling and errors:** 429, 5xx and network errors back off exponentially (30 s →
  10 min, honouring `Retry-After` if present). Polling stays at ≤ 10 per 5 min, a tenth of
  the limit. 401 refreshes the token once, then logs in again. Tokens live in memory only
  (a restart logs in again). Credentials sit in the gitignored config, like the MQTT ones.

### Core changes (as built in E1–E2)
- `core::charger`: `ChargerMode`, `ChargerReading`, `ChargerTarget::{Limit(Ampere), Pause}`,
  `ChargerWrite { current, ttl }` (what the Easee command takes), `ChargerWriteRecord`
  (persisted), `ChargerOverride`, `ChargerConfig`, and `Charger` (latest reading, override
  detection, and the charger's own actuator: TTL writes, refresh at `ttl / 2`, retry when a
  write isn't confirmed by observation 48 within `min_change_interval`, release = write
  `max_current` for 1 min, forget without writing on unplug or override).
- `Input::Charger(ChargerReading)`, `Input::SetChargerShedAllowed { allowed }`.
- Outputs: `WriteCharger { write, reason }`, `WouldWriteCharger { write, reason }`,
  `PersistChargerWrite(Option<ChargerWriteRecord>)` (startup safety; `None` once released),
  `PersistChargerShedAllowed(bool)`. The store keeps both in `settings` (`charger_write` =
  `"<amps> <at> <until>"`, `charger_shed_allowed` = `1`/`0`); no migration.
- Events: `GuardEvent::ChargerLimit { current, saving, projected, limit }` (lowering, a pause
  = 0 A, or a raise with negative saving), `GuardEvent::ChargerRelease { reason }`,
  `EventKind::ChargerOverride { current, until }`. `ReleaseReason` gains `CarDisconnected`
  and `ChargerUnavailable`. No `ChargerCommandFailed` event: a rejected command can only be
  seen by the adapter, which logs it (E3); the actuator retries unconfirmed writes anyway.
- `Status::charger` (`ChargerStatus`): freshness, availability, latest reading, the guard's
  `limit`, `shed_allowed`, `override_until`.
- `GuardState` gains `charger: Option<ChargerShed>` and a pause counter. The guard's model
  of the car: it draws `min(demand, current × V)`. `demand` is its own reported draw, unless
  it draws what the limit allows: then the limit holds it back and `demand` is assumed to be
  the ceiling (the user's charger limit). So a full car (drawing nothing) is released quickly,
  and raises are conservative otherwise.
- A raise waits for `restore_stagger` after the last release, like zone releases (s16: a raise
  10 s after a floor came back made the guard shed the floor again).
- Startup safety: in `Active` a persisted limit still in force is taken back into the guard,
  which keeps it until the first poll (no release-then-limit flapping, s22), then decides as
  usual. An expired one is just forgotten.
- Proptest: the target is always a pause (only when allowed) or whole amps in
  `[min_current, ceiling)`, and changes at most once per `min_change_interval`.

### Adapter: `energybrain-easee`
Same split as the MQTT crate:
- **pure layer:** request building, observation parsing → `ChargerReading`,
  `LimitCharger` → command body, the token state machine, the backoff schedule;
- **thin transport** (`reqwest`): a poll loop on `poll_interval` (default 30 s) that sends
  `Input`s into the app loop through a channel, and a command task that receives
  `LimitCharger`.
  The app wires it like the MQTT link. A missing `[ev]` section means no charger and no
  task.

```toml
[ev]
serial = "EH000000"            # placeholder; the real serial lives in the gitignored config
username = "…"                 # Easee account, gitignored config only
password = "…"
api_url = "https://api.easee.com"   # the sim's fake Easee in dev
phases = 1
min_current = 6
max_current = 32
pause_allowed = false
shed_allowed = true
poll_interval_secs = 30
ttl_minutes = 10
min_change_interval_secs = 120
max_raise_step = 4
min_pause_minutes = 15
max_pauses_per_hour = 2
stale_after_secs = 180
override_minutes = 120
```

### Milestones
Each ends with `just check` green and a commit.

**E0 — Discovery (user, before Active).**
1. Find the serial, and check that the observations endpoint works with your account (a
   `curl` recipe in `docs/easee.md`).
2. ✅ App mapping confirmed (2026-11-20): "Charger limit" = 47, "Temporary charging
   limit" = 48. The duration in the app and how a cleared 48 reads were skipped (see the
   override rule).
3. Optional, only needed to enable pausing: with the I-Pace plugged in, set 0 A and back
   to 10 A a few times, once after the car has slept for an hour. If it resumes every time,
   set `pause_allowed = true`. Until then pausing stays off and 6 A is the floor.

**E1 — Core types. ✅** `core::charger`: `ChargerMode` (with `car_connected`),
`ChargerReading`, `ChargerConfig` (voltage, `stale_after`), `Charger` (latest reading,
freshness, `available` = fresh + cloud-connected + not offline), `ChargerStatus`;
`Ampere × Volt`, `Kw / Volt`; `Input::Charger`, `EngineConfig::charger` (`None` until
E3), `Status::charger` (`null` without a charger). Outputs, events and `ChargerLimit` move
to E2, where they are first produced, so nothing downstream has to handle variants that
never occur.

**E2 — Guard + actuator. ✅** Charger stage in `GuardState`, observe-mode credit, hysteresis
and pause rules, TTL refresh, echo confirmation, leave-Active/shutdown release, startup
safety. Scenarios s15–s22 in `core/src/scenarios.rs` (a simulated car that follows its
offer 20 s later, polled every 30 s):
1. EV at 15 A in a cold evening → stepped down, floors untouched;
2. EV + floors over budget at 6 A → floors shed after the charger;
3. pause disallowed vs allowed;
4. charger stale → floors only;
5. observe mode with the EV → no write, would-limit events, floors not over-shed;
6. car unplugged while limited → no retry storm.
7. temporary limit set in the app while limited → override, charger left alone, floors shed;
8. restart mid-limit keeps it; leaving Active releases it.

**E3 — `energybrain-easee` + config. ✅** `api` (pure: observation parsing, lenient about
numbers sent as strings; command body in whole amps and whole minutes rounded up; command
outcome; token lifetime; backoff), `client` (login, refresh at 80 % of `expiresIn`,
re-login when a refresh is refused, one refresh-and-retry on 401, 429 → `RateLimited` with
`Retry-After`), `link` (polls at once and every `poll_interval`, backs off 30 s → 10 min on
failure, sends writes as they come, stops when the write channel closes). Tested against a
fake Easee on `axum`. `[ev]` config section (commented out in `config/example.toml`),
credentials also from `ENERGYBRAIN_EASEE_USERNAME`/`ENERGYBRAIN_EASEE_PASSWORD`;
`Config::easee` holds the API side, `engine.charger` the guard side. Required
observations: op mode and power; a missing cloud flag counts as connected.

**E4 — App wiring and surfaces. ✅** `Runtime::charger` holds the Easee `Link` (built in
`main.rs`, the only fallible part); `run()` spawns it, feeds its readings to the engine in the
`select!` loop and routes `WriteCharger` to it (`try_send`; the actuator retries unconfirmed
writes). On shutdown the release is flushed (write channel closed, link awaited up to 10 s).
HomeKit: `<prefix>/ev/shed_allowed` (+ `/set`) and `<prefix>/ev/limited`, only with `[ev]`.
`/metrics`: `energybrain_ev_available`, `energybrain_ev_power_kw`,
`energybrain_ev_offered_amps`, `energybrain_ev_limit_amps` (while limited; 0 = paused),
`energybrain_ev_writes_total`, `energybrain_ev_would_write_total`. `/api/state` carries the
charger section. `/healthz` stays 200 when only the charger is stale. Compose passes the
optional `ENERGYBRAIN_EASEE_*` variables (an empty variable counts as unset). `run()` is now a
thin wrapper boxing the shutdown future, so its body has one instantiation (coverage).

**E5 — Simulator.** The sim serves a fake Easee API (axum) backed by its EV model. The EV
then draws `min(offered, car_max) × 230 V`, ramping over ~20 s, with the TTL obeyed. This
replaces `sim/ev/set` power with a plug-in/plug-out control. `config/dev.toml` points
`api_url` at it.

**E6 — History page.** EV power stacked in the house-power chart, offered and limited
amps in their own small chart (store migration 3: charger columns in `samples`).

**Go-live:** at least a week of observe mode with the car charging on several evenings.
Check the would-limit events against reality, decide `pause_allowed` from E0, then enable
the charger in Active.

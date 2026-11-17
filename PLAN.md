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
- Custom web UI and a metrics stack (user runs none; observe mode is reviewed through
  `/api/events` and `/api/state`; `/metrics` is kept so Grafana can be added later)

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
                 SQLite store              mqtt adapter            /metrics, /api/state
            (hourly kWh, overrides,                                (axum) → VictoriaMetrics
             last written setpoints)                                       → Grafana
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
│   ├── http/      energybrain-http    axum: /healthz, /metrics, /api/state
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
  dozen events a day is negligible). Schema migrations run on open, with a
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
| EV charger (Easee Charge Lite) | New adapter crate for Easee's cloud API, a controllable load with a current cap; `PeakGuard` treats it as a sheddable load before floors |
| Optimiser (F) | `energybrain-planner` crate using `good_lp` + `microlp`, producing a per-hour plan the `PlanPolicy` follows, still subject to `PeakGuard` caps |
| Custom UI | Extend `energybrain-http` with endpoints and static assets |

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
- **Dev broker: mosquitto in an Apple container** (`container` CLI 1.0; Docker isn't
  installed). It's the same broker as production, needs no broker code, and is started by
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
  `mqtt.host = "127.0.0.1"`, no credentials, `http.listen = "127.0.0.1:8080"`,
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
  zone = "bad"; heater_kw = 1.5; setpoint = 26.0
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
  1. runs `container system start` (does nothing if already running);
  2. removes any old `energybrain-mqtt` container;
  3. runs `container run -d --name energybrain-mqtt -p 127.0.0.1:1883:1883`, with
     `deploy/dev/mosquitto` mounted as `/mosquitto/config`, using `eclipse-mosquitto:2`;
  4. removes `target/dev/energybrain.db*`;
  5. runs `cargo run -p energybrain-sim -- config/dev.toml config/sim.toml`.

  energybrain's MQTT connection already retries, so a broker that is still starting is
  harmless.
- `just dev-down`: stops and removes the broker container.
- `just sim <control> <value>` publishes `sim/<control>/set`, `just dev-pub <topic> <value>`
  anything, and `just dev-sub [topic]` watches the broker, all through `container exec` into the
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

# energybrain — working rules for Claude Code

The full design and milestone plan is in `PLAN.md`. Read it before starting work, and
keep it up to date if a decision changes.

## Commands
- `just check` — fmt check, clippy (`-D warnings`), all tests, coverage gate. Must be green before every commit.
- `just test` — `cargo test --workspace --all-features`
- `just cov` — `cargo llvm-cov --workspace --all-features --fail-under-lines 100 --ignore-filename-regex '(^|/)main\.rs$'`
- `just run` — run locally with `config/example.toml`
- `just arch` — render the architecture diagrams (`docs/architecture/`) and show them with kitty
  graphics (a new Rex tab inside Rex). **Update `system.d2`/`engine.d2` and re-run it whenever
  crates, `Input`/`Output` kinds, the engine pipeline or external systems change**; commit the
  `.d2` and `.svg` files together.
- `just dev` — energybrain + simulated house on accelerated time, against mosquitto in a
  container: Apple `container` on macOS, Docker elsewhere, `EB_RUNTIME=docker|container`
  overrides (`just dev-down` stops it; `just sim <control> <value>`, `just dev-pub`, `just dev-sub`
  poke and watch it). See `docs/dev.md`. The user runs these.

## Non-negotiables
- **100 % line coverage**, enforced. The only excluded file is `crates/app/src/main.rs`, which holds plumbing only. No coverage-exclusion attributes, no `cfg(coverage)`. If something can't be tested, refactor it.
- `energybrain-core` is pure: no tokio, no I/O, no reading the system clock. Time always comes in as an argument.
- Physical quantities use the unit newtypes (`Kw`, `Kwh`, `Celsius`, …), never bare `f64`, across module boundaries.
- Store timestamps in UTC. Local time (`Europe/Oslo` by default) is used only through `LocalCalendar`, for hour and day bucketing. Test DST days explicitly.
- No `_ =>` catch-all arms on `Input`, `Output` or `ProposalKind`.
- Never write setpoints in `Observe` or `Off` mode. One exception, agreed with the user: leaving
  `Active` first writes every zone we changed back to its base (the last act of `Active`), then
  switches mode. Every setpoint decision carries reasons.
- `unsafe_code = "forbid"`. Library crates use `thiserror`. No `unwrap()`/`expect()` in non-test code unless the invariant is documented in a comment right next to it.
- Don't add dependencies beyond those listed in PLAN.md §3 without recording why below.

## Workflow
- Work milestone by milestone (PLAN.md §5). Write tests together with the code.
- Commit at the end of each green milestone (or smaller green steps) with descriptive messages.
- M0 needs answers from the user (HAN payloads, Z-Wave topics, tariff table). Ask early, and record the answers in the section below and in `config/example.toml`.

## Environment facts (fill in during M0)
- MQTT broker: on the LAN, port 1883 (the real address lives only in the gitignored config;
  `config/example.toml` uses the placeholder `192.168.1.10`), no TLS, username + password. Credentials live only in a
  gitignored config file (`config/*.toml` except `example.toml` is ignored) — never commit them.
- HAN reader + topics + payload format: Tibber Pulse (local MQTT) on a **Kaifa MA304H3E**
  (3-phase, list id `KFM_001`, IT net: UL2 reads 0).
  Topic **`tibber`**, one complete HDLC frame (`7e … 7e`) per message:
  - 41 B list 1 every ~2.5 s: active import power (W, u32);
  - 123 B list 2 every 10 s: list id, meter id, type, P+, P−, Q+, Q− (W/var), IL1–3 (mA),
    UL1–3 (0.1 V);
  - list 3 hourly at hh:00:10: list 2 + meter clock + cumulative A+, A−, R+, R− (Wh);
  - ~400 B JSON `{"status":{…}}` from the Pulse itself — ignore.
  The DLMS header carries a 12-byte date-time in **local time with deviation unspecified**
  (`0x8000`); clock-status byte `0x00` in winter (DST bit `0x80` expected in summer).
- Zones (id, setpoint topic, power topic, nominal kW, priority): Heatit **Z-TRM3**, via Z-Wave JS UI
  in *value only* mode. Per device `zwave/<Room>/<Device>/`:
  `67/1/setpoint/1` (heating setpoint °C, plain number; write to `…/set`, echoed twice within 1 s),
  `50/1/value/66049` (power W, reported on change and roughly every 90 s, ~0.1 W when idle),
  `50/1/value/65537` (kWh total), `66/1/state` (1 = heating, 0 = idle), `64/1/mode` (1 = heat),
  `49/2` / `49/4` `Air_temperature` (room / floor sensor), `status` (`true` = alive).
  Devices (anonymised in the repo; real names only in the gitignored config):
  `Bathroom/Floor_bathroom`, `Hallway/Floor_hallway`, `Basement/Floor_basement`,
  `Guest/Floor_guest_1`, `Guest/Floor_guest_2`, `Laundry/Heating_cable` (measured ~575 W when on).
  Shed priority (lower sheds first, configurable): 1 `bathroom`, 2 `laundry`, 3 `hallway`,
  4 `guest_1`, 5 `guest_2`, 6 `basement` (already at 15 °C, little to gain). The guest
  zones are a separate unit, currently empty: shedding allowed, but must stay switchable per
  zone (HomeKit `shed_allowed`). Heater sizes unknown → learned from power reports
  (`nominal_kw` default 1 kW until learned). `shed_delta` 4 °C everywhere (Basement → 11 °C ok).
  `Living/Ceiling_light` is a ceiling-light switch: ignored.
- Grid company + capacity tiers: **Tensio TS**. Tiers (kW, kr/month): 0–2 131, 2–5 233, 5–10 397,
  10–15 585, 15–20 775, 20–25 964, 25–50 1655, 50–75 2599, 75–100 3544, 100–150 5117,
  150–200 7003, 200–300 10147, 300–400 13925, 400–500 17697, >500 21473.
  Energy charge (later): day 06–22 37.79 øre/kWh, night 22–06 23.79 øre/kWh (no separate weekend
  rate in the table; source: `docs/tier-table.md`). Capacity basis assumed **hourly** (not confirmed with Tensio).
- Target tier: **2–5 kW** (limit 5 kWh/h − 0.3 margin). Observe mode reports hours where it
  is unreachable (EV charging) to judge whether the target is realistic.
- Runtime host: Docker on amd64. No metrics stack: Grafana/VictoriaMetrics dropped from
  compose; observe mode is reviewed via `/api/events`.
- Other big loads: Easee Charge Lite EV charger (WiFi/cloud API, not controlled in phase 1),
  heat pump (not controllable). Both show up only in the whole-house meter reading.
- EV: Jaguar I-Pace on the Easee, charging **1-phase** (10 A ≈ 2.2–2.3 kW; 6 A min ≈ 1.4 kW,
  32 A max ≈ 7.4 kW). Usually run at 10–15 A from the Easee app. No departure target needed
  yet: the charger may be shed freely. Easee/Tibber smart charging is off (no competing
  controller).
- HomeKit: Homebridge **Easy MQTT** plugin. Zigbee2MQTT also exists but is unrelated.

## Decision log
- M1: `energybrain::version()` exists so the coverage gate has lines to measure (llvm-cov fails
  an empty workspace).
- Clippy `unwrap_used`/`expect_used` are warn (→ error under `-D warnings`); `clippy.toml` allows
  them in tests. A documented-invariant `expect` needs a local `#[allow]` plus the comment.
- HAN: new pure crate `energybrain-han` parses Kaifa HDLC/DLMS frames (CRC-16 hand-rolled, no new
  dependency). It replaces the plain-number/JSON HAN parsing originally planned for the mqtt crate.
- Echo detection: a reported setpoint within ±0.25 °C of our last write is an echo **regardless
  of elapsed time**, until a different value is reported. The learned base per zone is persisted.
  (Prevents learning a shed value as base after a restart.)
- `ZonePower` is a last-known value (Z-Wave reports on change); `nominal_kw` is only a fallback
  until the first power report arrives.
- Hour rollover does not force-release sheds; the budget resets and normal hysteresis decides.
  Every release (any time) goes through the `restore_stagger`.
- Shedding uses a per-zone `shed_delta`, default 4 °C, clamped to `min_setpoint`.
- While shed: a user setpoint above the cap → override (default 2 h, zone not shed); at or below
  the cap → becomes the new base, no override. An expired override's value remains the base.
- No seeding of earlier peaks: history starts on deploy day (fewer known days ⇒ more conservative).
- Tier bands are half-open `[from, to)`.
- Meter timestamps: power readings (list 1/2) use **receive time** from the app's `Clock`
  (staleness stays meaningful and consistent with ticks); the cumulative register (list 3)
  uses the **meter's own clock** (hh:00:00, converted from local time via the calendar's zone),
  so the hour value is `Measured` rather than interpolated across a 10 s delay.
- Hour energy is upserted: `Meter::advance` emits an `Estimated` value at rollover; the
  register later supersedes it with `Measured`/`Interpolated`.
- `clippy.toml` disallows `Timestamp::now`, `Zoned::now`, `SystemTime::now` workspace-wide;
  only `SystemClock` may opt out with a local `#[allow]`.
- Hour limit simplified (user decision): target tier bound, relaxed to today's highest
  completed hour if higher, minus margin. No month-end relaxation, no "tier lost" mode.
- Zones learn `heating_kw` (EMA of power reports > 50 W, persisted) and record per-zone hourly
  kWh from the thermostat's counter (`50/1/value/65537`).
- Persisted guard event log + `GET /api/events` replace dashboards for reviewing observe mode.
- Kaifa frame date-times are local wall-clock time; the han crate resolves them with the
  configured zone, using the DST status bit to pick the right side of the fall-back hour.
- Kaifa list 3 (real capture 2026-11-15 22:00:10 local, 157 B): the meter clock reads hh:00:10,
  but the registers are the values captured at the hour shift (Norwegian HAN spec), so
  `Register::captured_at` moves a clock within the first minute of an hour back to hh:00:00.
- HAN fixtures in `tests/fixtures/han/` are real frames with the meter serial replaced by
  `1234567890123456` and the FCS recomputed. Never commit the real serial.
- **Tests live inside each crate** (`#[cfg(test)]` modules, e.g. `core/src/scenarios.rs`), not in
  `tests/` dirs: an integration-test binary links a second instrumented copy of the crate, and
  llvm-cov then reports functions only one binary calls as uncovered.
- Scenario/replay tests are a closed-loop Rust simulation (house + meter + echoing thermostats)
  instead of CSV traces: typed, readable, and Active-mode feedback is real. Snapshots via `insta`
  (`INSTA_UPDATE=new`, review the `.snap.new`, then rename) — no `cargo-insta` needed.
- Guard credit (observe mode): a shed zone still drawing is credited its whole saving since the
  shed; when such a zone is released its saving is **banked** for the rest of the hour, otherwise
  observe mode re-sheds everything after the first release.
- `Engine::new(config, persisted, now)` and `Input::SetMode { at, mode }` take time explicitly;
  outputs gained `WouldWrite`, `PersistZoneBase`, `PersistMode`, `PersistShedAllowed`.
- Core derives `serde::Serialize` (serde is pure) on status/event types so MQTT
  `energybrain/status`, HTTP `/api/state` and the stored event log share one JSON shape, pinned by
  insta snapshots in `core/src/io.rs`. Timestamps serialize as RFC 3339 UTC.
- Units serialize as `{"value": 5.3, "unit": "kWh"}` (user request: bare numbers left readers
  guessing kW vs kWh). The unit string is the same literal `Display` uses (`Kwh::UNIT`). JSON
  field names don't repeat the unit: `heating_kw` → `heating_power`, `DayPeak`/`HourEnergy`
  `kwh` → `energy`, override event `value` → `setpoint` (`#[serde(rename)]` only; Rust names
  unchanged). Tier `price` is `"kr"` (per month, implied by context). Events stored before the
  change keep bare numbers; no migration (nothing deployed yet). The sim's `sim/state` uses the
  same shape (`power`, `loads`, `temperature`, `setpoint`, `outdoor`, `register`).
- `rumqttc`/`rumqttd` are used with `default-features = false` (no TLS; the broker is plain TCP).
- M7 MQTT tests: **embedded `rumqttd`** (in-process, free port, with username/password auth)
  — no Docker or testcontainers needed. Built from a TOML snippet since `rumqttd::Config` is serde.
- MQTT retained status topics are published only when their payload changes and all re-sent on
  every `ConnAck`. Setpoint writes go to `<setpoint>/set`, QoS 1, not retained.
- Coverage gotcha: llvm-cov's *summary* (what `--fail-under-lines` checks) sums each function's
  own line coverage, so a line holding only a `?` error branch after a closure (e.g. `})?;` closing
  a `query_map` closure) counts as missed even when `--show-missing-lines`/HTML show nothing. Fix
  by making the closure a named function so the `?` shares a line with covered code. To find such
  lines, export JSON (`cargo llvm-cov report --json`) and look for per-function zero regions.
- Upgraded to **jiff 1.0** (released 2026-11-15) across all crates: `Zoned` → `ZonedDateTime`,
  `to_zoned` → `to_zoned_datetime`. Keep every crate on the same jiff major.
- Store: per-zone state (base, last write, learned power, shed permission, override) lives in one
  `zones` row instead of separate `zone_writes`/`overrides` tables. Events are stored as their
  JSON plus indexed `at`/`kind`/`zone` columns.
- `/metrics` is rendered by hand in Prometheus text format from the latest `Status` plus write
  counters (`energybrain-http::Metrics`) instead of using `metrics` +
  `metrics-exporter-prometheus`: those need a process-global recorder, which is awkward to test
  and adds two dependencies. Metric names are unchanged from PLAN.md.
- `/healthz` is 503 with reasons when: no status yet, the last status is older than 30 s (engine
  loop stalled), the meter is stale/has no data, or MQTT is disconnected.
- App: `run()` creates the MQTT link itself (subscriptions come from the translator); MQTT
  events reach the `select!` loop through a forwarding task (cancel-safe), and the store runs on
  its own thread fed by a channel. `/api/events` opens a separate SQLite connection per query.
- Graceful stop (SIGTERM/Ctrl-C): `Engine::shutdown` restores shed zones in Active mode without
  changing the persisted mode, then MQTT is flushed (disconnect + wait), and the store thread and
  HTTP server are stopped.
- Deployment: `deploy/Dockerfile` (rust:1.100 build → distroless cc nonroot, `/data` volume,
  `/config/energybrain.toml`), `deploy/docker-compose.yml` (credentials from `deploy/.env`,
  both gitignored). CI builds the image; locally `just docker` works only where Docker is
  installed (the Linux dev machine, not the Mac). jiff's `tzdb-bundle-always` embeds the tz database so `Europe/Oslo` resolves in distroless.
- Releases: publishing a GitHub release runs `.github/workflows/release.yml`, which reuses
  `ci.yml` (`workflow_call`) as a gate, then pushes `deploy/Dockerfile` (amd64 only) to Docker Hub
  as `<DOCKER_USERNAME>/energybrain`. Releases are plain integers (user decision, no semver):
  git tag `vN` → image tags `N` and `latest`; a pre-release `vN-rc.1` → only `N-rc.1`. A first
  job rejects any other tag name, since metadata-action would otherwise push only `latest`. Needs repo secrets `DOCKER_USERNAME` and `DOCKER_PASSWORD` (same names as the
  user's other Docker Hub projects). Running
  the published image is documented in `docs/docker.md` with `docs/compose.yml`.
- Toolchain is **pinned** to an exact version in `rust-toolchain.toml` (now 1.100.0); CI installs
  it with `rustup toolchain install` and the Dockerfile uses the matching `rust:X.Y` image. A
  floating `stable` let CI pick up Rust 1.100 (new `clippy::assert_is_empty`) while local was
  1.97, so `just check` failed only on CI. Bump all three together, deliberately.
  `rust-version` in `Cargo.toml` stays the MSRV (1.97). Empty-collection asserts use
  `assert_eq!(x, Vec::<T>::new())`. After switching toolchains run a full `cargo llvm-cov clean`
  (not `--workspace`): stale test binaries from the old rustc are otherwise merged into the
  report and show up as hundreds of missed lines. 1.100 also maps implicit `else`/`?` branches
  to lines more precisely, which exposed 4 real gaps (fixed).
- Actuator: a write not yet echoed (`Zone::awaiting_echo`, not persisted, cleared by any
  setpoint report) counts as the thermostat's current setpoint for `min_interval`; after that
  it is presumed lost and the report wins (so it is retried). Without this, leaving Active or
  shutting down right after a shed write saw the stale pre-write report equal to the base and
  skipped the restore, leaving the zone shed (flaky e2e test on CI).
- Local dev (PLAN.md §7): production keeps mosquitto (no embedded broker); dev runs mosquitto
  in a container and an in-process simulated house on an accelerated clock.
- Dev runtime (user decision): the justfile supports both Apple `container` (default on macOS)
  and Docker (default elsewhere; the dev machine is now also Linux), chosen by `os()` and
  overridable with `EB_RUNTIME`. The only differences are `container system start` and
  `delete` vs `rm`.
- S1: `energybrain_han::encode` is the exact inverse of `parse` (it reproduces the real fixtures
  byte for byte). To make that possible `parse` now returns every value in the lists (Q±,
  per-phase currents/voltages as `Phases::One`/`Three`, R±), with new units `Kvar`, `Kvarh`,
  `Ampere`, `Volt` in core. The HDLC `frame` builder moved from tests into `hdlc.rs`.
- S3: `energybrain-sim` crate (dev only, no new dependencies: SplitMix64 is hand-rolled instead
  of `rand`). `House` is pure discrete-event code like the engine; its tests feed every
  published message through energybrain's real `Translator` (dev-dependency on
  `energybrain-mqtt`) so the simulated wire format can't drift from what energybrain parses.
- The simulator is a **developer tool for watching behaviour, not a test harness** (user
  decision): no end-to-end test of simulator + energybrain; its tests exist for the coverage gate
  and to keep its wire format in step with energybrain's parser.
- S4: the sim driver reuses `energybrain_mqtt::transport` (new `Publisher::publish_bytes` for
  binary HAN frames). `initial_mode` is written to the fresh dev database (`PersistMode`) before
  energybrain starts, instead of publishing `mode/set`, which could race energybrain's subscribe.
  `config/dev.toml` and `config/sim.toml` are committed (no credentials; un-ignored in
  `.gitignore`); a sim test asserts `dev.toml` has the same zones, topics and tariff as
  `example.toml`.
- S5: the sim driver drops what the house publishes while disconnected (rumqttc only drains its
  bounded request queue while connected, so a full queue would block the loop and Ctrl-C) and
  republishes every thermostat's retained values on each connect. Dev broker: anonymous mosquitto
  without persistence, recreated by every `just dev`.
- History page (PLAN.md §8, replaces Grafana): `GET /` serves a static page + vendored
  **uPlot 1.6.32** (MIT, `crates/http/assets/`, checksum-verified against jsdelivr and unpkg),
  embedded with `include_str!`. Not a Rust dependency; JS is outside the coverage gate, so it
  only draws: day/month bounds, hour limits, gaps, floors total, peaks, tier and the heatmap
  are computed in `energybrain-http::history` and snapshot-tested. To bump uPlot, replace the
  three files and the version check in `page.rs`.
- Store migration 2: `samples`/`zone_samples`, the first `PublishStatus` per minute (app clock
  time), pruned hourly after `store.sample_retention_days` (default 90). Sampling lives in
  `SqliteStore::apply`, not core. Hours, zone hours and events are still kept forever.
- `/api/day` and `/api/month` are columnar (`{"unit", "values": [...]}` per series) with
  RFC 3339 timestamps. They read through `HistoryQuery` (one SQLite connection per request,
  like `/api/events`). `blocking()` is generic per handler, so each handler's panic path
  needs its own test.
- Architecture diagrams (user request): D2 sources in `docs/architecture/` — `system.d2` and
  `engine.d2` hand-written, `crates.d2` generated by `scripts/arch-crates.sh` from
  `cargo metadata`. SVGs are committed (deterministic output, shown on GitHub); PNGs go to
  `target/arch/` and are shown by `scripts/kitty-show.sh` (kitty graphics; Rex doesn't accept
  SVG, and d2 ≥ 0.9 rasterises PNG itself, so no rsvg/chafa). Layout engine is ELK. `d2` is a
  dev tool, not a Rust dependency. A test in the app crate checks every crate has a box in
  `system.d2`.
- EV charger (PLAN.md §9): the charger is the guard's first stage, controlled only through
  Easee's TTL'd dynamic current (`ChargerWrite { current, ttl }`; release = `max_current` for
  1 min; pause = 0 A, off by default). Power per amp is configured (230 V × phases), not
  learned. A dynamic current below the user's charger limit that isn't one of our last two
  writes and isn't 0 is the user's temporary limit → override (2 h or until unplugged), never
  written over. The guard's car model: `draw = min(demand, current × V)`, where `demand` is the
  reported draw unless the car draws exactly what the limit allows (then the ceiling). Raises
  wait for `restore_stagger` like zone releases.

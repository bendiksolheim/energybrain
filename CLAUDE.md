# energybrain — working rules for Claude Code

The full design and milestone plan is in `PLAN.md`. Read it before starting work, and
keep it up to date if a decision changes.

## Commands
- `just check` — fmt check, clippy (`-D warnings`), all tests, coverage gate. Must be green before every commit.
- `just test` — `cargo test --workspace --all-features`
- `just cov` — `cargo llvm-cov --workspace --all-features --fail-under-lines 100 --ignore-filename-regex '(^|/)main\.rs$'`
- `just run` — run locally with `config/example.toml`

## Non-negotiables
- **100 % line coverage**, enforced. The only excluded file is `crates/app/src/main.rs`, which holds plumbing only. No coverage-exclusion attributes, no `cfg(coverage)`. If something can't be tested, refactor it.
- `energybrain-core` is pure: no tokio, no I/O, no reading the system clock. Time always comes in as an argument.
- Physical quantities use the unit newtypes (`Kw`, `Kwh`, `Celsius`, …), never bare `f64`, across module boundaries.
- Store timestamps in UTC. Local time (`Europe/Oslo` by default) is used only through `LocalCalendar`, for hour and day bucketing. Test DST days explicitly.
- No `_ =>` catch-all arms on `Input`, `Output` or `ProposalKind`.
- Never write setpoints in `Observe` or `Off` mode. Every setpoint decision carries reasons.
- `unsafe_code = "forbid"`. Library crates use `thiserror`. No `unwrap()`/`expect()` in non-test code unless the invariant is documented in a comment right next to it.
- Don't add dependencies beyond those listed in PLAN.md §3 without recording why below.

## Workflow
- Work milestone by milestone (PLAN.md §5). Write tests together with the code.
- Commit at the end of each green milestone (or smaller green steps) with descriptive messages.
- M0 needs answers from the user (HAN payloads, Z-Wave topics, tariff table). Ask early, and record the answers in the section below and in `config/example.toml`.

## Environment facts (fill in during M0)
- MQTT broker: `192.168.1.172:1883`, no TLS, username + password. Credentials live only in a
  gitignored config file (`config/*.toml` except `example.toml` is ignored) — never commit them.
- HAN reader + topics + payload format: Tibber Pulse (local MQTT) on a **Kaifa MA304H3E**
  (3-phase, list id `KFM_001`). Payloads are **raw binary HDLC/DLMS frames**, not numbers/JSON.
  Topic and hex samples: pending (M0 broker dump).
- Zones (id, setpoint topic, power topic, nominal kW, priority): Heatit **Z-TRM3**, via Z-Wave JS UI
  in *value only* mode. Topics/priorities: pending (M0 broker dump).
- Grid company + capacity tiers: **Tensio TS**. Tiers (kW, kr/month): 0–2 131, 2–5 233, 5–10 397,
  10–15 585, 15–20 775, 20–25 964, 25–50 1655, 50–75 2599, 75–100 3544, 100–150 5117,
  150–200 7003, 200–300 10147, 300–400 13925, 400–500 17697, >500 21473.
  Energy charge (later): day 06–22 37.79 øre/kWh, night 22–06 23.79 øre/kWh (no separate weekend
  rate in the table). Capacity basis assumed **hourly** (not confirmed with Tensio).
- Target tier: undecided; user aims for 2–5 kW.
- Runtime host: Docker on amd64. No metrics stack (Grafana/VictoriaMetrics) for now.
- Other big loads: Easee Charge Lite EV charger (WiFi/cloud API, not controlled in phase 1),
  heat pump (not controllable). Both show up only in the whole-house meter reading.
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

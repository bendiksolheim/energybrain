//! A simulated house for running energybrain on a laptop: a Kaifa HAN meter behind a Tibber
//! Pulse, a Heatit Z-TRM3 per floor, background load, a heat pump, appliances and an EV
//! charger. Everything goes over MQTT in the real devices' formats, so energybrain runs
//! unchanged against it. See PLAN.md §7.

pub mod config;
pub mod driver;
pub mod floor;
pub mod house;
pub mod loads;
pub mod rng;

pub use config::{SimConfig, SimConfigError, load, parse};
pub use house::House;

/// A message the house publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publish {
    /// Topic.
    pub topic: String,
    /// Payload: a HAN frame on the meter topic, UTF-8 text everywhere else.
    pub payload: Vec<u8>,
    /// Whether the broker should retain it.
    pub retain: bool,
}

impl Publish {
    /// A text message.
    pub fn text(topic: String, payload: impl Into<String>, retain: bool) -> Self {
        Self {
            topic,
            payload: payload.into().into_bytes(),
            retain,
        }
    }
}

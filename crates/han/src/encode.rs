//! Builds Kaifa HAN frames: the inverse of [`crate::parse`], used to simulate a meter.

use jiff::civil::DateTime;

use crate::hdlc;
use crate::{Details, HanError, MeterTime, Phases, Reading, Register};

/// LLC header, `data-notification` tag and long-invoke-id-and-priority, as Kaifa sends them.
const NOTIFICATION_HEADER: [u8; 8] = [0xe6, 0xe7, 0x00, 0x0f, 0x40, 0x00, 0x00, 0x00];
const NULL_DATA: u8 = 0x00;
const STRUCTURE: u8 = 0x02;
const OCTET_STRING: u8 = 0x09;
const DOUBLE_LONG_UNSIGNED: u8 = 0x06;
/// Longest text that fits a single-byte DLMS length.
const MAX_TEXT: usize = 0x7f;

/// Encodes `reading` as one HDLC frame, exactly as a Kaifa meter would send it.
///
/// Which list is sent follows from the reading: no details is list 1, details without a
/// register is list 2 and details with a register is list 3. A 1-phase or 3-phase list is
/// chosen by [`Details::phases`]. Values are rounded to the meter's resolution (1 W, 1 var,
/// 1 mA, 0.1 V, 1 Wh) and clamped to `0..=u32::MAX`; sub-second precision of date-times is
/// dropped. For every reading that [`crate::parse`] returns, `encode` reproduces the frame.
pub fn encode(reading: &Reading) -> Result<Vec<u8>, HanError> {
    let mut items = Vec::new();
    let count = match (&reading.details, &reading.register) {
        (None, None) => {
            push_u32(&mut items, reading.import_power.value(), 1000.0);
            1
        }
        (None, Some(_)) => return Err(HanError::RegisterWithoutDetails),
        (Some(details), register) => {
            push_details(&mut items, details, reading)?;
            let phases = match details.phases {
                Phases::One { .. } => 9,
                Phases::Three { .. } => 13,
            };
            match register {
                Some(register) => {
                    push_register(&mut items, register);
                    phases + 5
                }
                None => phases,
            }
        }
    };

    let mut info = NOTIFICATION_HEADER.to_vec();
    match &reading.sent {
        None => info.push(NULL_DATA),
        Some(sent) => push_time(&mut info, sent),
    }
    info.extend([STRUCTURE, count]);
    info.extend(items);
    Ok(hdlc::frame(&info))
}

fn push_details(out: &mut Vec<u8>, d: &Details, r: &Reading) -> Result<(), HanError> {
    push_text(out, &d.list_id)?;
    push_text(out, &d.meter_id)?;
    push_text(out, &d.meter_type)?;
    push_u32(out, r.import_power.value(), 1000.0);
    push_u32(out, d.export_power.value(), 1000.0);
    push_u32(out, d.reactive_import.value(), 1000.0);
    push_u32(out, d.reactive_export.value(), 1000.0);
    match d.phases {
        Phases::One { current, voltage } => {
            push_u32(out, current.value(), 1000.0);
            push_u32(out, voltage.value(), 10.0);
        }
        Phases::Three { currents, voltages } => {
            for current in currents {
                push_u32(out, current.value(), 1000.0);
            }
            for voltage in voltages {
                push_u32(out, voltage.value(), 10.0);
            }
        }
    }
    Ok(())
}

fn push_register(out: &mut Vec<u8>, r: &Register) {
    push_time(out, &r.clock);
    push_u32(out, r.import.value(), 1000.0);
    push_u32(out, r.export.value(), 1000.0);
    push_u32(out, r.reactive_import.value(), 1000.0);
    push_u32(out, r.reactive_export.value(), 1000.0);
}

/// A `double-long-unsigned` holding `value × scale`, rounded.
fn push_u32(out: &mut Vec<u8>, value: f64, scale: f64) {
    // `as` saturates (NaN becomes 0), which is the clamping documented on `encode`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let raw = (value * scale).round() as u32;
    out.push(DOUBLE_LONG_UNSIGNED);
    out.extend(raw.to_be_bytes());
}

fn push_text(out: &mut Vec<u8>, text: &str) -> Result<(), HanError> {
    let len = u8::try_from(text.len())
        .ok()
        .filter(|&n| usize::from(n) <= MAX_TEXT)
        .ok_or(HanError::TextTooLong(text.len()))?;
    out.extend([OCTET_STRING, len]);
    out.extend(text.as_bytes());
    Ok(())
}

/// A 12-byte COSEM date-time octet-string.
fn push_time(out: &mut Vec<u8>, t: &MeterTime) {
    out.extend([OCTET_STRING, 12]);
    out.extend(time_bytes(t));
}

fn time_bytes(t: &MeterTime) -> [u8; 12] {
    let d: DateTime = t.datetime;
    let [y0, y1] = d.year().to_be_bytes();
    let [dev0, dev1] = t.deviation.unwrap_or(i16::MIN).to_be_bytes();
    let status = match t.dst {
        None => 0xff,
        Some(false) => 0x00,
        Some(true) => 0x80,
    };
    [
        y0,
        y1,
        d.month().unsigned_abs(),
        d.day().unsigned_abs(),
        d.weekday().to_monday_one_offset().unsigned_abs(),
        d.hour().unsigned_abs(),
        d.minute().unsigned_abs(),
        d.second().unsigned_abs(),
        0xff, // hundredths: not specified
        dev0,
        dev1,
        status,
    ]
}

#[cfg(test)]
mod tests {
    use energybrain_core::units::{Ampere, Kvar, Kvarh, Kw, Kwh, Volt};
    use jiff::civil::date;
    use proptest::prelude::*;

    use super::*;
    use crate::parse;

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/../../tests/fixtures/han/{name}.hex",
            env!("CARGO_MANIFEST_DIR")
        );
        let hex = std::fs::read_to_string(path).unwrap();
        let hex = hex.trim();
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn reproduces_every_real_frame_byte_for_byte() {
        for name in [
            "kaifa_ma304h3e_list1",
            "kaifa_ma304h3e_list2",
            "kaifa_ma304h3e_list3",
        ] {
            let frame = fixture(name);
            let reading = parse(&frame).unwrap();
            assert_eq!(encode(&reading).unwrap(), frame, "{name}");
        }
    }

    fn time(dst: Option<bool>, deviation: Option<i16>) -> MeterTime {
        MeterTime {
            datetime: date(2026, 7, 1).at(12, 0, 10, 0),
            deviation,
            dst,
        }
    }

    fn details(phases: Phases) -> Details {
        Details {
            list_id: "KFM_001".into(),
            meter_id: "1234567890123456".into(),
            meter_type: "MA105H2E".into(),
            export_power: Kw::new(0.2),
            reactive_import: Kvar::new(0.1),
            reactive_export: Kvar::ZERO,
            phases,
        }
    }

    #[test]
    fn one_phase_list_3_without_header_time() {
        let reading = Reading {
            sent: None,
            import_power: Kw::new(1.5),
            details: Some(details(Phases::One {
                current: Ampere::new(6.5),
                voltage: Volt::new(231.2),
            })),
            register: Some(Register {
                clock: time(Some(true), Some(-120)),
                import: Kwh::new(1000.0),
                export: Kwh::new(5.0),
                reactive_import: Kvarh::new(1.0),
                reactive_export: Kvarh::ZERO,
            }),
        };
        assert_eq!(parse(&encode(&reading).unwrap()).unwrap(), reading);
    }

    #[test]
    fn date_time_fields() {
        // 2026-07-01 is a Wednesday (3); summer time sets the DST bit.
        let b = time_bytes(&time(Some(true), None));
        assert_eq!(b, [0x07, 0xea, 7, 1, 3, 12, 0, 10, 0xff, 0x80, 0x00, 0x80]);
        assert_eq!(time_bytes(&time(None, Some(-60)))[9..], [0xff, 0xc4, 0xff]);
        assert_eq!(time_bytes(&time(Some(false), None))[11], 0x00);
    }

    #[test]
    fn values_are_rounded_and_clamped() {
        let encoded = |kw: f64| {
            let mut out = Vec::new();
            push_u32(&mut out, kw, 1000.0);
            out
        };
        assert_eq!(encoded(1.2344), [0x06, 0, 0, 0x04, 0xd2]);
        assert_eq!(encoded(-1.0), [0x06, 0, 0, 0, 0]);
        assert_eq!(encoded(f64::NAN), [0x06, 0, 0, 0, 0]);
        assert_eq!(encoded(1e12), [0x06, 0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn rejects_what_a_meter_cannot_send() {
        let register = Register {
            clock: time(None, None),
            import: Kwh::ZERO,
            export: Kwh::ZERO,
            reactive_import: Kvarh::ZERO,
            reactive_export: Kvarh::ZERO,
        };
        let orphan = Reading {
            sent: None,
            import_power: Kw::ZERO,
            details: None,
            register: Some(register),
        };
        assert_eq!(encode(&orphan), Err(HanError::RegisterWithoutDetails));

        let mut d = details(Phases::One {
            current: Ampere::ZERO,
            voltage: Volt::ZERO,
        });
        d.meter_type = "x".repeat(128);
        let long = Reading {
            sent: None,
            import_power: Kw::ZERO,
            details: Some(d.clone()),
            register: None,
        };
        assert_eq!(encode(&long), Err(HanError::TextTooLong(128)));
        d.meter_type = "x".repeat(300);
        let longer = Reading {
            details: Some(d.clone()),
            ..long.clone()
        };
        assert_eq!(encode(&longer), Err(HanError::TextTooLong(300)));
        d.meter_type = "x".repeat(127);
        let longest = Reading {
            details: Some(d),
            ..long
        };
        assert!(parse(&encode(&longest).unwrap()).is_ok());
    }

    fn arb_time() -> impl Strategy<Value = MeterTime> {
        (
            2000i16..2100,
            1i8..=12,
            1i8..=28,
            0i8..24,
            0i8..60,
            0i8..60,
            prop::option::of(-720i16..=720),
            prop::option::of(any::<bool>()),
        )
            .prop_map(|(y, mo, d, h, mi, s, deviation, dst)| MeterTime {
                datetime: date(y, mo, d).at(h, mi, s, 0),
                deviation,
                dst,
            })
    }

    /// Readings built from wire integers, so they survive the meter's resolution exactly.
    fn arb_reading() -> impl Strategy<Value = Reading> {
        let milli = |raw: u32| f64::from(raw) / 1000.0;
        let phases = prop_oneof![
            (any::<u32>(), 0u32..5000).prop_map(move |(i, u)| Phases::One {
                current: Ampere::new(milli(i)),
                voltage: Volt::new(f64::from(u) / 10.0),
            }),
            (any::<[u32; 3]>(), any::<[u16; 3]>()).prop_map(move |(i, u)| Phases::Three {
                currents: i.map(|v| Ampere::new(milli(v))),
                voltages: u.map(|v| Volt::new(f64::from(v) / 10.0)),
            }),
        ];
        let details = (any::<[u32; 3]>(), "[A-Z0-9_]{0,20}", phases).prop_map(
            move |([p, q_in, q_out], id, phases)| Details {
                list_id: "KFM_001".into(),
                meter_id: id,
                meter_type: "MA304H3E".into(),
                export_power: Kw::new(milli(p)),
                reactive_import: Kvar::new(milli(q_in)),
                reactive_export: Kvar::new(milli(q_out)),
                phases,
            },
        );
        let register = (arb_time(), any::<[u32; 4]>()).prop_map(move |(clock, r)| Register {
            clock,
            import: Kwh::new(milli(r[0])),
            export: Kwh::new(milli(r[1])),
            reactive_import: Kvarh::new(milli(r[2])),
            reactive_export: Kvarh::new(milli(r[3])),
        });
        let list = prop_oneof![
            Just((None, None)),
            details.clone().prop_map(|d| (Some(d), None)),
            (details, register).prop_map(|(d, r)| (Some(d), Some(r))),
        ];
        (prop::option::of(arb_time()), any::<u32>(), list).prop_map(
            move |(sent, power, (details, register))| Reading {
                sent,
                import_power: Kw::new(milli(power)),
                details,
                register,
            },
        )
    }

    proptest! {
        #[test]
        fn parse_inverts_encode(reading in arb_reading()) {
            let frame = encode(&reading).unwrap();
            prop_assert_eq!(parse(&frame).unwrap(), reading);
        }
    }
}

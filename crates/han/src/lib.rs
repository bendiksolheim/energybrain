//! Pure parser for Kaifa HAN frames (HDLC + DLMS/COSEM) as forwarded raw by a Tibber Pulse.
//!
//! Kaifa meters send three lists, distinguished by the number of elements in the body:
//!
//! | List | 1-phase | 3-phase | Content |
//! |---|---|---|---|
//! | 1 | 1 | 1 | active import power (W) |
//! | 2 | 9 | 13 | list id, meter id, meter type, P+, P−, Q+, Q− (W/var), currents, voltages |
//! | 3 | 14 | 18 | list 2 + meter clock, A+, A−, R+, R− (Wh/varh) |
//!
//! Date-times are COSEM 12-byte values. Kaifa sends **local wall-clock time** with the
//! UTC deviation unspecified, so turning them into instants needs the time zone; the
//! clock-status DST bit (0x80) resolves the repeated hour on the fall-back day.

mod dlms;
mod hdlc;

use energybrain_core::time::HourStart;
use energybrain_core::units::{Kw, Kwh};
use jiff::civil::DateTime;
use jiff::tz::{Offset, TimeZone};
use jiff::{SignedDuration, Timestamp};

use dlms::Value;

/// Why a payload could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HanError {
    /// The payload is not an HDLC frame at all (e.g. the Pulse's own JSON status).
    #[error("not an HDLC frame")]
    NotHdlc,
    /// The frame ended early.
    #[error("frame truncated")]
    Truncated,
    /// Not an HDLC type-3 frame.
    #[error("unsupported HDLC frame format {0:#06x}")]
    BadFormat(u16),
    /// A segmented frame (Kaifa never sends these).
    #[error("segmented HDLC frames are not supported")]
    Segmented,
    /// The declared length does not match.
    #[error("frame declares {declared} bytes but has {actual}")]
    Length {
        /// Length in the format field.
        declared: usize,
        /// Actual length between the flags.
        actual: usize,
    },
    /// Frame checksum mismatch.
    #[error("frame checksum mismatch")]
    FrameChecksum,
    /// Header checksum mismatch.
    #[error("header checksum mismatch")]
    HeaderChecksum,
    /// An HDLC address longer than four bytes.
    #[error("malformed HDLC address")]
    BadAddress,
    /// Not a DLMS data-notification.
    #[error("not a DLMS data-notification")]
    UnsupportedApdu,
    /// A DLMS type this parser does not handle.
    #[error("unsupported DLMS type tag {0:#04x}")]
    UnsupportedType(u8),
    /// A date-time that is malformed or does not exist.
    #[error("bad date-time: {0}")]
    BadDateTime(String),
    /// A body with an element count that matches no known list.
    #[error("unknown list with {0} elements")]
    UnknownList(usize),
    /// An element of the wrong type.
    #[error("element {0} has an unexpected type")]
    UnexpectedValue(usize),
}

/// A COSEM date-time as sent by the meter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterTime {
    /// Wall-clock date and time.
    pub datetime: DateTime,
    /// Minutes of local time from UTC, if the meter specifies it.
    pub deviation: Option<i16>,
    /// Whether daylight saving time was in effect, if the meter says.
    pub dst: Option<bool>,
}

impl MeterTime {
    fn parse(b: [u8; 12]) -> Result<Self, HanError> {
        let year = i16::from_be_bytes([b[0], b[1]]);
        let bad = |e: jiff::Error| HanError::BadDateTime(e.to_string());
        let narrow =
            |v: u8| i8::try_from(v).map_err(|_| HanError::BadDateTime(format!("field {v}")));
        let datetime = DateTime::new(
            year,
            narrow(b[2])?,
            narrow(b[3])?,
            narrow(b[5])?,
            narrow(b[6])?,
            narrow(b[7])?,
            0,
        )
        .map_err(bad)?;
        let deviation = match i16::from_be_bytes([b[9], b[10]]) {
            i16::MIN => None,
            d => Some(d),
        };
        let dst = (b[11] != 0xff).then_some(b[11] & 0x80 != 0);
        Ok(Self {
            datetime,
            deviation,
            dst,
        })
    }

    /// The instant this wall-clock time denotes in `tz`.
    ///
    /// With a deviation, it is used directly (DLMS: local = UTC − deviation minutes).
    /// Otherwise the zone decides; in the repeated fall-back hour the DST bit picks the
    /// summer (earlier) or winter (later) instant.
    pub fn to_timestamp(&self, tz: &TimeZone) -> Result<Timestamp, HanError> {
        let bad = |e: jiff::Error| HanError::BadDateTime(e.to_string());
        if let Some(d) = self.deviation {
            let offset = Offset::from_seconds(-i32::from(d) * 60).map_err(bad)?;
            return offset.to_timestamp(self.datetime).map_err(bad);
        }
        let amb = tz.to_ambiguous_timestamp(self.datetime);
        match self.dst {
            Some(true) => amb.earlier(),
            Some(false) => amb.later(),
            None => amb.compatible(),
        }
        .map_err(bad)
    }
}

/// Identity and export power, from lists 2 and 3.
#[derive(Debug, Clone, PartialEq)]
pub struct Details {
    /// List version identifier, e.g. `KFM_001`.
    pub list_id: String,
    /// Meter serial (GSIN).
    pub meter_id: String,
    /// Meter type, e.g. `MA304H3E`.
    pub meter_type: String,
    /// Active export power.
    pub export_power: Kw,
}

/// The cumulative registers, from list 3.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Register {
    /// The meter's clock when it sent the list.
    pub clock: MeterTime,
    /// Cumulative active import (A+).
    pub import: Kwh,
    /// Cumulative active export (A−).
    pub export: Kwh,
}

impl Register {
    /// The instant the register values apply to. Per the Norwegian HAN specification the
    /// list-3 registers are captured at the hour shift and sent a few seconds later, so a
    /// clock within the first minute of an hour is moved back to the hour start.
    pub fn captured_at(&self, tz: &TimeZone) -> Result<Timestamp, HanError> {
        let at = self.clock.to_timestamp(tz)?;
        let hour = HourStart::containing(at);
        Ok(
            if at.duration_since(hour.start()) < SignedDuration::from_mins(1) {
                hour.start()
            } else {
                at
            },
        )
    }
}

/// One decoded HAN frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// The notification's header time, if present.
    pub sent: Option<MeterTime>,
    /// Active import power.
    pub import_power: Kw,
    /// Lists 2 and 3 only.
    pub details: Option<Details>,
    /// List 3 only.
    pub register: Option<Register>,
}

/// Decodes one MQTT payload holding a single HAN frame.
pub fn parse(payload: &[u8]) -> Result<Reading, HanError> {
    let info = hdlc::information(payload)?;
    let note = dlms::notification(info)?;
    let sent = note.date_time.map(MeterTime::parse).transpose()?;
    let Value::Structure(items) = note.body else {
        return Err(HanError::UnexpectedValue(0));
    };
    let watts =
        |i: usize| -> Result<Kw, HanError> { Ok(Kw::new(f64::from(u32_at(&items, i)?) / 1000.0)) };
    let (details, register) = match items.len() {
        1 => (None, None),
        n @ (9 | 13 | 14 | 18) => {
            let details = Details {
                list_id: text_at(&items, 0)?,
                meter_id: text_at(&items, 1)?,
                meter_type: text_at(&items, 2)?,
                export_power: watts(4)?,
            };
            let register = if matches!(n, 14 | 18) {
                let base = n - 5;
                let wh = |i: usize| -> Result<Kwh, HanError> {
                    Ok(Kwh::new(f64::from(u32_at(&items, i)?) / 1000.0))
                };
                Some(Register {
                    clock: MeterTime::parse(time_at(&items, base)?)?,
                    import: wh(base + 1)?,
                    export: wh(base + 2)?,
                })
            } else {
                None
            };
            (Some(details), register)
        }
        n => return Err(HanError::UnknownList(n)),
    };
    let first = if details.is_some() { 3 } else { 0 };
    Ok(Reading {
        sent,
        import_power: watts(first)?,
        details,
        register,
    })
}

fn u32_at(items: &[Value], i: usize) -> Result<u32, HanError> {
    match items.get(i) {
        Some(Value::U32(v)) => Ok(*v),
        _ => Err(HanError::UnexpectedValue(i)),
    }
}

fn bytes_at(items: &[Value], i: usize) -> Result<&[u8], HanError> {
    match items.get(i) {
        Some(Value::Bytes(b)) => Ok(b),
        _ => Err(HanError::UnexpectedValue(i)),
    }
}

fn text_at(items: &[Value], i: usize) -> Result<String, HanError> {
    Ok(String::from_utf8_lossy(bytes_at(items, i)?).into_owned())
}

fn time_at(items: &[Value], i: usize) -> Result<[u8; 12], HanError> {
    bytes_at(items, i)?
        .try_into()
        .map_err(|_| HanError::UnexpectedValue(i))
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;
    use proptest::prelude::*;

    use super::*;
    use crate::hdlc::tests::frame;

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

    fn oslo() -> TimeZone {
        TimeZone::get("Europe/Oslo").unwrap()
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn real_list_1() {
        let r = parse(&fixture("kaifa_ma304h3e_list1")).unwrap();
        assert_eq!(r.import_power, Kw::new(6.371));
        assert_eq!(r.details, None);
        assert_eq!(r.register, None);
        let sent = r.sent.unwrap();
        assert_eq!(sent.deviation, None);
        assert_eq!(sent.dst, Some(false));
    }

    #[test]
    fn real_list_2() {
        let r = parse(&fixture("kaifa_ma304h3e_list2")).unwrap();
        let d = r.details.unwrap();
        assert_eq!(d.list_id, "KFM_001");
        assert_eq!(d.meter_id, "1234567890123456");
        assert_eq!(d.meter_type, "MA304H3E");
        assert_eq!(d.export_power, Kw::ZERO);
        assert!(r.import_power > Kw::ZERO);
        assert_eq!(r.register, None);
    }

    #[test]
    fn real_list_3() {
        let r = parse(&fixture("kaifa_ma304h3e_list3")).unwrap();
        assert_eq!(r.import_power, Kw::new(5.616));
        let reg = r.register.unwrap();
        assert_eq!(reg.import, Kwh::new(263_743.035));
        assert_eq!(reg.export, Kwh::ZERO);
        assert_eq!(reg.clock.datetime, date(2026, 11, 15).at(22, 0, 10, 0));
        // 22:00:10 local in November is 21:00:10Z; the registers belong to 21:00:00Z.
        assert_eq!(
            reg.clock.to_timestamp(&oslo()).unwrap(),
            ts("2026-11-15T21:00:10Z")
        );
        assert_eq!(
            reg.captured_at(&oslo()).unwrap(),
            ts("2026-11-15T21:00:00Z")
        );
        assert_eq!(
            r.sent.unwrap().to_timestamp(&oslo()).unwrap(),
            ts("2026-11-15T21:00:10Z")
        );
    }

    fn meter_time(dt: DateTime, deviation: Option<i16>, dst: Option<bool>) -> MeterTime {
        MeterTime {
            datetime: dt,
            deviation,
            dst,
        }
    }

    #[test]
    fn fall_back_hour_is_resolved_by_the_dst_bit() {
        // 2026-10-25 02:30 local happens twice in Oslo.
        let dt = date(2026, 10, 25).at(2, 30, 0, 0);
        let tz = oslo();
        let summer = meter_time(dt, None, Some(true)).to_timestamp(&tz).unwrap();
        let winter = meter_time(dt, None, Some(false)).to_timestamp(&tz).unwrap();
        assert_eq!(summer, ts("2026-10-25T00:30:00Z"));
        assert_eq!(winter, ts("2026-10-25T01:30:00Z"));
        assert_eq!(
            meter_time(dt, None, None).to_timestamp(&tz).unwrap(),
            summer
        );
    }

    #[test]
    fn explicit_deviation_wins() {
        // DLMS deviation -60 means local = UTC + 1 h.
        let dt = date(2026, 11, 15).at(22, 0, 0, 0);
        let t = meter_time(dt, Some(-60), None)
            .to_timestamp(&oslo())
            .unwrap();
        assert_eq!(t, ts("2026-11-15T21:00:00Z"));
        let err = meter_time(dt, Some(i16::MAX), None).to_timestamp(&oslo());
        assert!(matches!(err, Err(HanError::BadDateTime(_))));
    }

    #[test]
    fn unresolvable_instants_are_errors() {
        let dt = DateTime::MAX;
        let err = meter_time(dt, None, Some(false))
            .to_timestamp(&TimeZone::fixed(Offset::from_hours(-2).unwrap()));
        assert!(matches!(err, Err(HanError::BadDateTime(_))));
        let reg = Register {
            clock: meter_time(dt, None, None),
            import: Kwh::ZERO,
            export: Kwh::ZERO,
        };
        assert!(
            reg.captured_at(&TimeZone::fixed(Offset::from_hours(-2).unwrap()))
                .is_err()
        );
    }

    #[test]
    fn register_late_in_the_hour_is_not_moved() {
        let reg = Register {
            clock: meter_time(date(2026, 11, 15).at(22, 5, 0, 0), None, Some(false)),
            import: Kwh::ZERO,
            export: Kwh::ZERO,
        };
        assert_eq!(
            reg.captured_at(&oslo()).unwrap(),
            ts("2026-11-15T21:05:00Z")
        );
    }

    #[test]
    fn meter_time_fields() {
        let mut b = [0x07, 0xea, 11, 15, 7, 22, 0, 10, 0xff, 0x80, 0x00, 0xff];
        let t = MeterTime::parse(b).unwrap();
        assert_eq!(t.dst, None);
        b[11] = 0x80;
        assert_eq!(MeterTime::parse(b).unwrap().dst, Some(true));
        b[9] = 0xff;
        b[10] = 0xc4;
        assert_eq!(MeterTime::parse(b).unwrap().deviation, Some(-60));
        b[2] = 13;
        assert!(matches!(MeterTime::parse(b), Err(HanError::BadDateTime(_))));
        b[2] = 0xff;
        assert_eq!(
            MeterTime::parse(b),
            Err(HanError::BadDateTime("field 255".into()))
        );
    }

    /// A data-notification body without a header time.
    fn body(items: &[u8]) -> Vec<u8> {
        let mut v = vec![0xe6, 0xe7, 0x00, 0x0f, 0x40, 0, 0, 0, 0x00];
        v.extend(items);
        frame(&v)
    }

    fn u32_item(v: u32) -> Vec<u8> {
        let mut out = vec![0x06];
        out.extend(v.to_be_bytes());
        out
    }

    fn text_item(s: &str) -> Vec<u8> {
        let mut out = vec![0x09, u8::try_from(s.len()).unwrap()];
        out.extend(s.as_bytes());
        out
    }

    /// A 1-phase list 2 (9 elements), optionally extended to list 3 (14 elements).
    fn one_phase(list3: bool) -> Vec<u8> {
        let mut items = vec![0x02, if list3 { 14 } else { 9 }];
        items.extend(text_item("KFM_001"));
        items.extend(text_item("1234567890123456"));
        items.extend(text_item("MA105H2E"));
        for v in [1500, 200, 0, 0, 6000, 2300] {
            items.extend(u32_item(v));
        }
        if list3 {
            items.extend([
                0x09, 0x0c, 0x07, 0xea, 11, 15, 7, 22, 0, 10, 0xff, 0x80, 0x00, 0x00,
            ]);
            for v in [1_000_000, 5_000, 0, 0] {
                items.extend(u32_item(v));
            }
        }
        body(&items)
    }

    #[test]
    fn one_phase_lists() {
        let r = parse(&one_phase(false)).unwrap();
        assert_eq!(r.sent, None);
        assert_eq!(r.import_power, Kw::new(1.5));
        assert_eq!(r.details.unwrap().export_power, Kw::new(0.2));
        let r = parse(&one_phase(true)).unwrap();
        let reg = r.register.unwrap();
        assert_eq!(reg.import, Kwh::new(1000.0));
        assert_eq!(reg.export, Kwh::new(5.0));
    }

    #[test]
    fn rejects_unexpected_bodies() {
        assert_eq!(
            parse(&body(&u32_item(5))),
            Err(HanError::UnexpectedValue(0))
        );
        let mut two = vec![0x02, 2];
        two.extend(u32_item(1));
        two.extend(u32_item(2));
        assert_eq!(parse(&body(&two)), Err(HanError::UnknownList(2)));
        assert_eq!(
            parse(&body(&[0x02, 1, 0x11, 5])),
            Err(HanError::UnexpectedValue(0))
        );
        // List 2 whose meter id is a number.
        let mut l2 = vec![0x02, 9];
        l2.extend(text_item("KFM_001"));
        for v in [7, 0, 1500, 0, 0, 0, 0, 0] {
            l2.extend(u32_item(v));
        }
        assert_eq!(parse(&body(&l2)), Err(HanError::UnexpectedValue(1)));
    }

    #[test]
    fn rejects_list_3_with_a_bad_clock() {
        let mut items = vec![0x02, 14];
        items.extend(text_item("KFM_001"));
        items.extend(text_item("1234567890123456"));
        items.extend(text_item("MA105H2E"));
        for v in [1500, 200, 0, 0, 6000, 2300] {
            items.extend(u32_item(v));
        }
        items.extend(text_item("short clock"));
        for v in [1, 2, 3, 4] {
            items.extend(u32_item(v));
        }
        assert_eq!(parse(&body(&items)), Err(HanError::UnexpectedValue(9)));
    }

    #[test]
    fn json_status_is_not_a_frame() {
        assert_eq!(parse(br#"{"status":{"rssi":-57}}"#), Err(HanError::NotHdlc));
    }

    #[test]
    fn header_time_errors_propagate() {
        let mut v = vec![0xe6, 0xe7, 0x00, 0x0f, 0x40, 0, 0, 0, 0x09, 0x0c];
        v.extend([0x07, 0xea, 13, 15, 7, 22, 0, 10, 0xff, 0x80, 0x00, 0x00]);
        v.extend([0x02, 1]);
        v.extend(u32_item(1));
        assert!(matches!(parse(&frame(&v)), Err(HanError::BadDateTime(_))));
    }

    #[test]
    fn error_messages() {
        let all = [
            (HanError::NotHdlc, "not an HDLC frame"),
            (HanError::Truncated, "frame truncated"),
            (
                HanError::BadFormat(0x500e),
                "unsupported HDLC frame format 0x500e",
            ),
            (
                HanError::Segmented,
                "segmented HDLC frames are not supported",
            ),
            (
                HanError::Length {
                    declared: 3,
                    actual: 2,
                },
                "frame declares 3 bytes but has 2",
            ),
            (HanError::FrameChecksum, "frame checksum mismatch"),
            (HanError::HeaderChecksum, "header checksum mismatch"),
            (HanError::BadAddress, "malformed HDLC address"),
            (HanError::UnsupportedApdu, "not a DLMS data-notification"),
            (
                HanError::UnsupportedType(0x17),
                "unsupported DLMS type tag 0x17",
            ),
            (HanError::BadDateTime("x".into()), "bad date-time: x"),
            (HanError::UnknownList(2), "unknown list with 2 elements"),
            (
                HanError::UnexpectedValue(4),
                "element 4 has an unexpected type",
            ),
        ];
        for (e, msg) in all {
            assert_eq!(e.to_string(), msg);
        }
    }

    proptest! {
        /// Garbage never panics.
        #[test]
        fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
            let _ = parse(&bytes);
        }

        /// Corrupting a real frame never panics and never yields a different power silently
        /// (the checksum catches it).
        #[test]
        fn corrupted_frames_are_rejected(i in 1usize..156, flip in 1u8..=255) {
            let mut f = fixture("kaifa_ma304h3e_list3");
            f[i] ^= flip;
            prop_assert!(parse(&f).is_err());
        }
    }
}

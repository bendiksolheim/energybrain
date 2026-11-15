//! The DLMS/COSEM data-notification inside the HDLC information field.

use crate::HanError;
use crate::hdlc::Cursor;

/// LLC header for a response from the meter.
const LLC: [u8; 3] = [0xe6, 0xe7, 0x00];
/// `data-notification` APDU tag.
const DATA_NOTIFICATION: u8 = 0x0f;

/// A decoded DLMS value (the subset meters send on the HAN port).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// `structure` or `array`.
    Structure(Vec<Value>),
    /// `octet-string` or `visible-string`.
    Bytes(Vec<u8>),
    /// `double-long-unsigned`.
    U32(u32),
    /// `double-long`.
    I32(i32),
    /// `long-unsigned`.
    U16(u16),
    /// `long`.
    I16(i16),
    /// `unsigned` or `enum`.
    U8(u8),
    /// `integer`.
    I8(i8),
}

/// A data-notification: optional header date-time and the notification body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The 12-byte COSEM date-time from the header, if present.
    pub date_time: Option<[u8; 12]>,
    /// The body (normally a structure).
    pub body: Value,
}

/// Decodes the information field of a HAN frame.
pub fn notification(info: &[u8]) -> Result<Notification, HanError> {
    let mut c = Cursor::new(info);
    if c.array::<3>()? != LLC || c.u8()? != DATA_NOTIFICATION {
        return Err(HanError::UnsupportedApdu);
    }
    c.take(4)?; // long-invoke-id-and-priority
    // The header time is either null-data (0x00) or a 12-byte octet-string.
    let date_time = match c.u8()? {
        0x00 => None,
        0x09 => match c.u8()? {
            12 => Some(c.array()?),
            n => return Err(HanError::BadDateTime(format!("header length {n}"))),
        },
        tag => {
            return Err(HanError::BadDateTime(format!("header tag {tag:#04x}")));
        }
    };
    let body = value(&mut c)?;
    Ok(Notification { date_time, body })
}

/// Decodes one value.
fn value(c: &mut Cursor<'_>) -> Result<Value, HanError> {
    let tag = c.u8()?;
    Ok(match tag {
        0x01 | 0x02 => {
            let n = c.u8()?;
            Value::Structure((0..n).map(|_| value(c)).collect::<Result<_, _>>()?)
        }
        0x09 | 0x0a => {
            let n = c.u8()?;
            if n >= 0x80 {
                return Err(HanError::UnsupportedType(tag));
            }
            Value::Bytes(c.take(usize::from(n))?.to_vec())
        }
        0x06 => Value::U32(u32::from_be_bytes(c.array()?)),
        0x05 => Value::I32(i32::from_be_bytes(c.array()?)),
        0x12 => Value::U16(u16::from_be_bytes(c.array()?)),
        0x10 => Value::I16(i16::from_be_bytes(c.array()?)),
        0x11 | 0x16 => Value::U8(c.u8()?),
        0x0f => Value::I8(i8::from_be_bytes(c.array()?)),
        other => return Err(HanError::UnsupportedType(other)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(body: &[u8]) -> Vec<u8> {
        let mut v = vec![0xe6, 0xe7, 0x00, 0x0f, 0x40, 0, 0, 0, 0x00];
        v.extend(body);
        v
    }

    #[test]
    fn decodes_every_supported_type() {
        let body = [
            0x02, 0x0a, //
            0x09, 0x02, b'h', b'i', //
            0x0a, 0x01, b'x', //
            0x06, 0, 0, 1, 0, //
            0x05, 0xff, 0xff, 0xff, 0xfe, //
            0x12, 1, 0, //
            0x10, 0xff, 0xff, //
            0x11, 7, //
            0x16, 3, //
            0x0f, 0xfe, //
            0x01, 0x00,
        ];
        let n = notification(&note(&body)).unwrap();
        assert_eq!(n.date_time, None);
        assert_eq!(
            n.body,
            Value::Structure(vec![
                Value::Bytes(b"hi".to_vec()),
                Value::Bytes(b"x".to_vec()),
                Value::U32(256),
                Value::I32(-2),
                Value::U16(256),
                Value::I16(-1),
                Value::U8(7),
                Value::U8(3),
                Value::I8(-2),
                Value::Structure(vec![]),
            ])
        );
    }

    #[test]
    fn header_date_time() {
        let mut v = vec![0xe6, 0xe7, 0x00, 0x0f, 0x40, 0, 0, 0, 0x09, 0x0c];
        v.extend([7; 12]);
        v.extend([0x11, 1]);
        let n = notification(&v).unwrap();
        assert_eq!(n.date_time, Some([7; 12]));
        let bad_len = [0xe6, 0xe7, 0x00, 0x0f, 0x40, 0, 0, 0, 0x09, 0x05];
        assert_eq!(
            notification(&bad_len),
            Err(HanError::BadDateTime("header length 5".into()))
        );
        let bad_tag = [0xe6, 0xe7, 0x00, 0x0f, 0x40, 0, 0, 0, 0x05];
        assert_eq!(
            notification(&bad_tag),
            Err(HanError::BadDateTime("header tag 0x05".into()))
        );
    }

    #[test]
    fn rejects_other_apdus_and_types() {
        assert_eq!(
            notification(&[0xe6, 0xe7, 0x00, 0xc4]),
            Err(HanError::UnsupportedApdu)
        );
        assert_eq!(
            notification(&[0xe6, 0xe6, 0x00, 0x0f]),
            Err(HanError::UnsupportedApdu)
        );
        assert_eq!(
            notification(&note(&[0x17, 0])),
            Err(HanError::UnsupportedType(0x17))
        );
        assert_eq!(
            notification(&note(&[0x09, 0x81, 0])),
            Err(HanError::UnsupportedType(0x09))
        );
        assert_eq!(notification(&note(&[0x06, 0])), Err(HanError::Truncated));
        assert_eq!(
            notification(&note(&[0x02, 2, 0x11, 1])),
            Err(HanError::Truncated)
        );
    }
}

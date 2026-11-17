//! HDLC framing (IEC 62056-46) as used on the Norwegian HAN port.

use crate::HanError;

const FLAG: u8 = 0x7e;
const FORMAT_TYPE_3: u16 = 0xa000;
const SEGMENTED: u16 = 0x0800;
const LENGTH_MASK: u16 = 0x07ff;

/// CRC-16/X-25 (the HDLC FCS): reflected polynomial 0x1021, init and xorout 0xffff.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xffff;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x8408
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A bounds-checked reader over a byte slice.
#[derive(Debug)]
pub struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// A reader at the start of `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Bytes consumed so far.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Reads `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], HanError> {
        let end = self.pos.checked_add(n).ok_or(HanError::Truncated)?;
        let slice = self.bytes.get(self.pos..end).ok_or(HanError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    /// Reads one byte.
    pub fn u8(&mut self) -> Result<u8, HanError> {
        Ok(self.take(1)?[0])
    }

    /// Reads a fixed-size array.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], HanError> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
}

/// Validates one HDLC frame and returns its information field.
pub fn information(frame: &[u8]) -> Result<&[u8], HanError> {
    let [FLAG, inner @ .., FLAG] = frame else {
        return Err(HanError::NotHdlc);
    };
    let mut c = Cursor::new(inner);
    let format = u16::from_be_bytes(c.array()?);
    if format & 0xf000 != FORMAT_TYPE_3 {
        return Err(HanError::BadFormat(format));
    }
    if format & SEGMENTED != 0 {
        return Err(HanError::Segmented);
    }
    let declared = usize::from(format & LENGTH_MASK);
    if declared != inner.len() {
        return Err(HanError::Length {
            declared,
            actual: inner.len(),
        });
    }
    // Declared length was checked, and every field below is read through the cursor.
    let (body, fcs) = inner.split_at(inner.len().saturating_sub(2));
    if u16::from_le_bytes(Cursor::new(fcs).array()?) != crc16(body) {
        return Err(HanError::FrameChecksum);
    }
    address(&mut c)?;
    address(&mut c)?;
    c.u8()?; // control
    let header_end = c.pos();
    let hcs = u16::from_le_bytes(c.array()?);
    if hcs != crc16(&inner[..header_end]) {
        return Err(HanError::HeaderChecksum);
    }
    body.get(c.pos()..).ok_or(HanError::Truncated)
}

/// Wraps an information field in a frame as a Kaifa meter sends it: destination address
/// 0x01, source address 0x02 0x01, control 0x10 (UI frame), header and frame checksums.
pub fn frame(info: &[u8]) -> Vec<u8> {
    let len = 2 + 1 + 2 + 1 + 2 + info.len() + 2;
    // Callers keep frames far below the 11-bit length (`encode` limits each text field to
    // 127 bytes, so its largest frame is under 600 bytes), so masking loses nothing.
    #[allow(clippy::cast_possible_truncation)]
    let len = len as u16 & LENGTH_MASK;
    let mut inner = (FORMAT_TYPE_3 | len).to_be_bytes().to_vec();
    inner.extend([0x01, 0x02, 0x01, 0x10]);
    let hcs = crc16(&inner);
    inner.extend(hcs.to_le_bytes());
    inner.extend(info);
    let fcs = crc16(&inner);
    inner.extend(fcs.to_le_bytes());
    let mut out = vec![FLAG];
    out.extend(inner);
    out.push(FLAG);
    out
}

/// Skips an HDLC address (1–4 bytes; the last has its low bit set).
fn address(c: &mut Cursor<'_>) -> Result<(), HanError> {
    for _ in 0..4 {
        if c.u8()? & 1 == 1 {
            return Ok(());
        }
    }
    Err(HanError::BadAddress)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recomputes the FCS after a test has tampered with the frame.
    fn refcs(f: &mut [u8]) {
        let n = f.len();
        let fcs = crc16(&f[1..n - 3]).to_le_bytes();
        f[n - 3] = fcs[0];
        f[n - 2] = fcs[1];
    }

    #[test]
    fn crc_check_value() {
        // Standard CRC-16/X-25 check value.
        assert_eq!(crc16(b"123456789"), 0x906e);
    }

    #[test]
    fn extracts_the_information_field() {
        let f = frame(&[0xaa, 0xbb]);
        assert_eq!(information(&f).unwrap(), &[0xaa, 0xbb]);
    }

    #[test]
    fn rejects_non_frames() {
        assert_eq!(information(b"{\"status\":{}}"), Err(HanError::NotHdlc));
        assert_eq!(information(&[]), Err(HanError::NotHdlc));
        assert_eq!(information(&[FLAG, FLAG]), Err(HanError::Truncated));
    }

    #[test]
    fn rejects_bad_format_segmentation_and_length() {
        let mut f = frame(&[0xaa]);
        f[1] = 0x50;
        assert!(matches!(
            information(&f),
            Err(HanError::BadFormat(0x5000..=0x5fff))
        ));
        let mut f = frame(&[0xaa]);
        f[1] |= 0x08;
        assert_eq!(information(&f), Err(HanError::Segmented));
        let mut f = frame(&[0xaa]);
        f[2] += 1;
        assert_eq!(
            information(&f),
            Err(HanError::Length {
                declared: 12,
                actual: 11
            })
        );
    }

    #[test]
    fn rejects_bad_checksums() {
        let mut f = frame(&[0xaa]);
        f[9] ^= 0xff;
        assert_eq!(information(&f), Err(HanError::FrameChecksum));
        let mut f = frame(&[0xaa]);
        f[6] ^= 0xff; // control byte: header checksum now wrong
        refcs(&mut f);
        assert_eq!(information(&f), Err(HanError::HeaderChecksum));
    }

    #[test]
    fn rejects_unterminated_addresses() {
        // Format + four address bytes without a terminator, then a valid FCS.
        let mut inner = vec![0xa0, 0x08, 0x02, 0x02, 0x02, 0x02];
        let fcs = crc16(&inner);
        inner.extend(fcs.to_le_bytes());
        let mut f = vec![FLAG];
        f.extend(inner);
        f.push(FLAG);
        assert_eq!(information(&f), Err(HanError::BadAddress));
    }

    #[test]
    fn rejects_frames_too_short_for_their_header() {
        // Valid length and FCS, but the source address runs into the FCS.
        let mut inner = vec![0xa0, 0x05, 0x01];
        let fcs = crc16(&inner);
        inner.extend(fcs.to_le_bytes());
        let mut f = vec![FLAG];
        f.extend(inner);
        f.push(FLAG);
        assert_eq!(information(&f), Err(HanError::Truncated));
    }

    #[test]
    fn cursor_reports_overflow_as_truncation() {
        let mut c = Cursor::new(&[1, 2]);
        assert_eq!(c.take(usize::MAX), Err(HanError::Truncated));
        assert_eq!(c.u8(), Ok(1));
        assert_eq!(c.array::<2>(), Err(HanError::Truncated));
    }
}

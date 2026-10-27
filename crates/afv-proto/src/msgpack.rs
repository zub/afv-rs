//! The small subset of MessagePack the protocol uses.
//!
//! Encoding always picks the smallest representation, matching msgpack-c (the C++ client)
//! and MessagePack-CSharp (the server). Decoding accepts every width the format allows.

use crate::{Error, Result};

pub(crate) fn write_array_len(out: &mut Vec<u8>, len: usize) -> Result<()> {
    match len {
        0..=15 => out.push(0x90 | len as u8),
        16..=0xffff => {
            out.push(0xdc);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        _ => {
            let len = u32::try_from(len).map_err(|_| Error::TooLarge("array"))?;
            out.push(0xdd);
            out.extend_from_slice(&len.to_be_bytes());
        }
    }
    Ok(())
}

pub(crate) fn write_uint(out: &mut Vec<u8>, v: u64) {
    match v {
        0..=0x7f => out.push(v as u8),
        0x80..=0xff => out.extend_from_slice(&[0xcc, v as u8]),
        0x100..=0xffff => {
            out.push(0xcd);
            out.extend_from_slice(&(v as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(0xce);
            out.extend_from_slice(&(v as u32).to_be_bytes());
        }
        _ => {
            out.push(0xcf);
            out.extend_from_slice(&v.to_be_bytes());
        }
    }
}

pub(crate) fn write_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let len = s.len();
    match len {
        0..=31 => out.push(0xa0 | len as u8),
        32..=0xff => out.extend_from_slice(&[0xd9, len as u8]),
        0x100..=0xffff => {
            out.push(0xda);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        _ => {
            let len = u32::try_from(len).map_err(|_| Error::TooLarge("string"))?;
            out.push(0xdb);
            out.extend_from_slice(&len.to_be_bytes());
        }
    }
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

pub(crate) fn write_bin(out: &mut Vec<u8>, b: &[u8]) -> Result<()> {
    let len = b.len();
    match len {
        0..=0xff => out.extend_from_slice(&[0xc4, len as u8]),
        0x100..=0xffff => {
            out.push(0xc5);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        _ => {
            let len = u32::try_from(len).map_err(|_| Error::TooLarge("binary"))?;
            out.push(0xc6);
            out.extend_from_slice(&len.to_be_bytes());
        }
    }
    out.extend_from_slice(b);
    Ok(())
}

pub(crate) fn write_bool(out: &mut Vec<u8>, v: bool) {
    out.push(if v { 0xc3 } else { 0xc2 });
}

pub(crate) fn write_f32(out: &mut Vec<u8>, v: f32) {
    out.push(0xca);
    out.extend_from_slice(&v.to_be_bytes());
}

/// Zero-copy cursor over a MessagePack buffer.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn peek(&self) -> Result<u8> {
        self.buf.get(self.pos).copied().ok_or(Error::Truncated)
    }

    fn be<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("take returned N bytes"))
    }

    fn len_u8(&mut self) -> Result<usize> {
        Ok(self.byte()? as usize)
    }

    fn len_u16(&mut self) -> Result<usize> {
        Ok(u16::from_be_bytes(self.be()?) as usize)
    }

    fn len_u32(&mut self) -> Result<usize> {
        Ok(u32::from_be_bytes(self.be()?) as usize)
    }

    /// Reads an array header and checks it has at least `min` elements.
    pub(crate) fn array(&mut self, min: u32) -> Result<u32> {
        let m = self.byte()?;
        let len = match m {
            0x90..=0x9f => (m & 0x0f) as u32,
            0xdc => self.len_u16()? as u32,
            0xdd => self.len_u32()? as u32,
            _ => return Err(Error::UnexpectedType { expected: "array", marker: m }),
        };
        if len < min {
            return Err(Error::ArrayTooShort { expected: min, found: len });
        }
        Ok(len)
    }

    pub(crate) fn str(&mut self) -> Result<&'a str> {
        let m = self.byte()?;
        let len = match m {
            0xa0..=0xbf => (m & 0x1f) as usize,
            0xd9 => self.len_u8()?,
            0xda => self.len_u16()?,
            0xdb => self.len_u32()?,
            _ => return Err(Error::UnexpectedType { expected: "string", marker: m }),
        };
        std::str::from_utf8(self.take(len)?).map_err(|_| Error::InvalidUtf8)
    }

    /// Reads a `bin`, also accepting `str` (older MessagePack encoders use str for raw bytes).
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8]> {
        let m = self.byte()?;
        let len = match m {
            0xc4 | 0xd9 => self.len_u8()?,
            0xc5 | 0xda => self.len_u16()?,
            0xc6 | 0xdb => self.len_u32()?,
            0xa0..=0xbf => (m & 0x1f) as usize,
            _ => return Err(Error::UnexpectedType { expected: "binary", marker: m }),
        };
        self.take(len)
    }

    pub(crate) fn int(&mut self) -> Result<i128> {
        let m = self.byte()?;
        Ok(match m {
            0x00..=0x7f => m as i128,
            0xe0..=0xff => (m as i8) as i128,
            0xcc => self.byte()? as i128,
            0xcd => u16::from_be_bytes(self.be()?) as i128,
            0xce => u32::from_be_bytes(self.be()?) as i128,
            0xcf => u64::from_be_bytes(self.be()?) as i128,
            0xd0 => (self.byte()? as i8) as i128,
            0xd1 => i16::from_be_bytes(self.be()?) as i128,
            0xd2 => i32::from_be_bytes(self.be()?) as i128,
            0xd3 => i64::from_be_bytes(self.be()?) as i128,
            _ => return Err(Error::UnexpectedType { expected: "integer", marker: m }),
        })
    }

    pub(crate) fn uint<T: TryFrom<i128>>(&mut self, what: &'static str) -> Result<T> {
        T::try_from(self.int()?).map_err(|_| Error::IntegerOutOfRange(what))
    }

    pub(crate) fn bool(&mut self) -> Result<bool> {
        match self.byte()? {
            0xc2 => Ok(false),
            0xc3 => Ok(true),
            m => Err(Error::UnexpectedType { expected: "bool", marker: m }),
        }
    }

    /// Reads a float, also accepting integers.
    pub(crate) fn float(&mut self) -> Result<f64> {
        match self.peek()? {
            0xca => {
                self.pos += 1;
                Ok(f32::from_be_bytes(self.be()?) as f64)
            }
            0xcb => {
                self.pos += 1;
                Ok(f64::from_be_bytes(self.be()?))
            }
            _ => self.int().map(|i| i as f64),
        }
    }

    /// Skips one complete value of any type.
    pub(crate) fn skip(&mut self) -> Result<()> {
        let m = self.byte()?;
        let (skip_bytes, children): (usize, usize) = match m {
            0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => (0, 0),
            0x80..=0x8f => (0, 2 * (m & 0x0f) as usize),
            0x90..=0x9f => (0, (m & 0x0f) as usize),
            0xa0..=0xbf => ((m & 0x1f) as usize, 0),
            0xc4 | 0xd9 => (self.len_u8()?, 0),
            0xc5 | 0xda => (self.len_u16()?, 0),
            0xc6 | 0xdb => (self.len_u32()?, 0),
            0xc7 => (self.len_u8()? + 1, 0),
            0xc8 => (self.len_u16()? + 1, 0),
            0xc9 => (self.len_u32()? + 1, 0),
            0xca => (4, 0),
            0xcb => (8, 0),
            0xcc | 0xd0 => (1, 0),
            0xcd | 0xd1 => (2, 0),
            0xce | 0xd2 => (4, 0),
            0xcf | 0xd3 => (8, 0),
            0xd4 => (2, 0),
            0xd5 => (3, 0),
            0xd6 => (5, 0),
            0xd7 => (9, 0),
            0xd8 => (17, 0),
            0xdc => (0, self.len_u16()?),
            0xdd => (0, self.len_u32()?),
            0xde => (0, 2 * self.len_u16()?),
            0xdf => (0, 2 * self.len_u32()?),
            0xc1 => return Err(Error::UnexpectedType { expected: "any value", marker: m }),
        };
        self.take(skip_bytes)?;
        for _ in 0..children {
            self.skip()?;
        }
        Ok(())
    }

    /// Skips the elements of an array beyond the `read` already consumed.
    pub(crate) fn skip_rest(&mut self, len: u32, read: u32) -> Result<()> {
        for _ in read..len {
            self.skip()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uint_uses_smallest_encoding() {
        let cases: &[(u64, &[u8])] = &[
            (0, &[0x00]),
            (127, &[0x7f]),
            (128, &[0xcc, 0x80]),
            (256, &[0xcd, 0x01, 0x00]),
            (122_800_000, &[0xce, 0x07, 0x51, 0xc7, 0x80]),
            (1 << 32, &[0xcf, 0, 0, 0, 1, 0, 0, 0, 0]),
        ];
        for (v, expected) in cases {
            let mut out = Vec::new();
            write_uint(&mut out, *v);
            assert_eq!(&out, expected, "encoding {v}");
            assert_eq!(Reader::new(&out).uint::<u64>("test").unwrap(), *v);
        }
    }

    #[test]
    fn str_lengths() {
        for len in [0usize, 31, 32, 255, 256] {
            let s = "x".repeat(len);
            let mut out = Vec::new();
            write_str(&mut out, &s).unwrap();
            assert_eq!(Reader::new(&out).str().unwrap(), s);
        }
    }

    #[test]
    fn skip_nested() {
        // [ {"a": [1, 2.5]}, nil, "x" ] followed by 7
        let data = [0x93, 0x81, 0xa1, b'a', 0x92, 0x01, 0xcb, 0x40, 0x04, 0, 0, 0, 0, 0, 0, 0xc0, 0xa1, b'x', 0x07];
        let mut r = Reader::new(&data);
        r.skip().unwrap();
        assert_eq!(r.uint::<u8>("test").unwrap(), 7);
    }

    #[test]
    fn truncated_input_is_an_error() {
        let mut r = Reader::new(&[0xcd, 0x01]);
        assert!(matches!(r.int(), Err(Error::Truncated)));
    }
}

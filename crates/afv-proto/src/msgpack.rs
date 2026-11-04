//! The small subset of MessagePack the protocol uses.
//!
//! Encoding always picks the smallest representation, matching msgpack-c (the C++ client)
//! and MessagePack-CSharp (the server). Decoding accepts every width the format allows.

use std::mem::size_of;

use crate::{Error, Result};

/// Format markers from the MessagePack specification. The "fix" formats carry a small value or
/// length in their low bits.
mod marker {
    pub const POSITIVE_FIXINT_LAST: u8 = 0x7f;
    pub const FIXMAP: u8 = 0x80;
    pub const FIXMAP_LAST: u8 = 0x8f;
    pub const FIXARRAY: u8 = 0x90;
    pub const FIXARRAY_LAST: u8 = 0x9f;
    pub const FIXSTR: u8 = 0xa0;
    pub const FIXSTR_LAST: u8 = 0xbf;
    pub const NIL: u8 = 0xc0;
    pub const NEVER_USED: u8 = 0xc1;
    pub const FALSE: u8 = 0xc2;
    pub const TRUE: u8 = 0xc3;
    pub const BIN8: u8 = 0xc4;
    pub const BIN16: u8 = 0xc5;
    pub const BIN32: u8 = 0xc6;
    pub const EXT8: u8 = 0xc7;
    pub const EXT16: u8 = 0xc8;
    pub const EXT32: u8 = 0xc9;
    pub const FLOAT32: u8 = 0xca;
    pub const FLOAT64: u8 = 0xcb;
    pub const UINT8: u8 = 0xcc;
    pub const UINT16: u8 = 0xcd;
    pub const UINT32: u8 = 0xce;
    pub const UINT64: u8 = 0xcf;
    pub const INT8: u8 = 0xd0;
    pub const INT16: u8 = 0xd1;
    pub const INT32: u8 = 0xd2;
    pub const INT64: u8 = 0xd3;
    pub const FIXEXT1: u8 = 0xd4;
    pub const FIXEXT2: u8 = 0xd5;
    pub const FIXEXT4: u8 = 0xd6;
    pub const FIXEXT8: u8 = 0xd7;
    pub const FIXEXT16: u8 = 0xd8;
    pub const STR8: u8 = 0xd9;
    pub const STR16: u8 = 0xda;
    pub const STR32: u8 = 0xdb;
    pub const ARRAY16: u8 = 0xdc;
    pub const ARRAY32: u8 = 0xdd;
    pub const MAP16: u8 = 0xde;
    pub const MAP32: u8 = 0xdf;
    pub const NEGATIVE_FIXINT: u8 = 0xe0;
    /// The length bits of a fixarray or fixmap marker…
    pub const FIX_COLLECTION_LEN: u8 = 0x0f;
    /// …and of a fixstr marker.
    pub const FIXSTR_LEN: u8 = 0x1f;
    /// Extension values carry a type byte before their data.
    pub const EXT_TYPE_LEN: usize = 1;
}

use marker::*;

/// Deepest nesting [`Reader::skip`] follows. The protocol nests three levels (a DTO, its
/// transceiver list, one transceiver); anything far deeper is hostile, and following it would
/// recurse without bound.
const MAX_SKIP_DEPTH: usize = 32;

pub(crate) fn write_array_len(out: &mut Vec<u8>, len: usize) -> Result<()> {
    if len <= FIX_COLLECTION_LEN as usize {
        out.push(FIXARRAY | len as u8);
    } else if let Ok(len) = u16::try_from(len) {
        out.push(ARRAY16);
        out.extend_from_slice(&len.to_be_bytes());
    } else {
        let len = u32::try_from(len).map_err(|_| Error::TooLarge("array"))?;
        out.push(ARRAY32);
        out.extend_from_slice(&len.to_be_bytes());
    }
    Ok(())
}

pub(crate) fn write_uint(out: &mut Vec<u8>, v: u64) {
    if v <= POSITIVE_FIXINT_LAST as u64 {
        out.push(v as u8);
    } else if let Ok(v) = u8::try_from(v) {
        out.extend_from_slice(&[UINT8, v]);
    } else if let Ok(v) = u16::try_from(v) {
        out.push(UINT16);
        out.extend_from_slice(&v.to_be_bytes());
    } else if let Ok(v) = u32::try_from(v) {
        out.push(UINT32);
        out.extend_from_slice(&v.to_be_bytes());
    } else {
        out.push(UINT64);
        out.extend_from_slice(&v.to_be_bytes());
    }
}

pub(crate) fn write_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let len = s.len();
    if len <= FIXSTR_LEN as usize {
        out.push(FIXSTR | len as u8);
    } else if let Ok(len) = u8::try_from(len) {
        out.extend_from_slice(&[STR8, len]);
    } else if let Ok(len) = u16::try_from(len) {
        out.push(STR16);
        out.extend_from_slice(&len.to_be_bytes());
    } else {
        let len = u32::try_from(len).map_err(|_| Error::TooLarge("string"))?;
        out.push(STR32);
        out.extend_from_slice(&len.to_be_bytes());
    }
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

pub(crate) fn write_bin(out: &mut Vec<u8>, b: &[u8]) -> Result<()> {
    let len = b.len();
    if let Ok(len) = u8::try_from(len) {
        out.extend_from_slice(&[BIN8, len]);
    } else if let Ok(len) = u16::try_from(len) {
        out.push(BIN16);
        out.extend_from_slice(&len.to_be_bytes());
    } else {
        let len = u32::try_from(len).map_err(|_| Error::TooLarge("binary"))?;
        out.push(BIN32);
        out.extend_from_slice(&len.to_be_bytes());
    }
    out.extend_from_slice(b);
    Ok(())
}

pub(crate) fn write_bool(out: &mut Vec<u8>, v: bool) {
    out.push(if v { TRUE } else { FALSE });
}

pub(crate) fn write_f32(out: &mut Vec<u8>, v: f32) {
    out.push(FLOAT32);
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

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
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

    /// Reads an array header and checks it has at least `min` elements. Every element takes at
    /// least one byte, so a length beyond the bytes left is truncated input; rejecting it here
    /// lets callers size buffers by the length without trusting it.
    pub(crate) fn array(&mut self, min: u32) -> Result<u32> {
        let m = self.byte()?;
        let len = match m {
            FIXARRAY..=FIXARRAY_LAST => (m & FIX_COLLECTION_LEN) as u32,
            ARRAY16 => self.len_u16()? as u32,
            ARRAY32 => self.len_u32()? as u32,
            _ => return Err(Error::UnexpectedType { expected: "array", marker: m }),
        };
        if len < min {
            return Err(Error::ArrayTooShort { expected: min, found: len });
        }
        if len as usize > self.remaining() {
            return Err(Error::Truncated);
        }
        Ok(len)
    }

    pub(crate) fn str(&mut self) -> Result<&'a str> {
        let m = self.byte()?;
        let len = match m {
            FIXSTR..=FIXSTR_LAST => (m & FIXSTR_LEN) as usize,
            STR8 => self.len_u8()?,
            STR16 => self.len_u16()?,
            STR32 => self.len_u32()?,
            _ => return Err(Error::UnexpectedType { expected: "string", marker: m }),
        };
        std::str::from_utf8(self.take(len)?).map_err(|_| Error::InvalidUtf8)
    }

    /// Reads a `bin`, also accepting `str` (older MessagePack encoders use str for raw bytes).
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8]> {
        let m = self.byte()?;
        let len = match m {
            BIN8 | STR8 => self.len_u8()?,
            BIN16 | STR16 => self.len_u16()?,
            BIN32 | STR32 => self.len_u32()?,
            FIXSTR..=FIXSTR_LAST => (m & FIXSTR_LEN) as usize,
            _ => return Err(Error::UnexpectedType { expected: "binary", marker: m }),
        };
        self.take(len)
    }

    pub(crate) fn int(&mut self) -> Result<i128> {
        let m = self.byte()?;
        Ok(match m {
            0..=POSITIVE_FIXINT_LAST => m as i128,
            NEGATIVE_FIXINT..=u8::MAX => (m as i8) as i128,
            UINT8 => self.byte()? as i128,
            UINT16 => u16::from_be_bytes(self.be()?) as i128,
            UINT32 => u32::from_be_bytes(self.be()?) as i128,
            UINT64 => u64::from_be_bytes(self.be()?) as i128,
            INT8 => (self.byte()? as i8) as i128,
            INT16 => i16::from_be_bytes(self.be()?) as i128,
            INT32 => i32::from_be_bytes(self.be()?) as i128,
            INT64 => i64::from_be_bytes(self.be()?) as i128,
            _ => return Err(Error::UnexpectedType { expected: "integer", marker: m }),
        })
    }

    pub(crate) fn uint<T: TryFrom<i128>>(&mut self, what: &'static str) -> Result<T> {
        T::try_from(self.int()?).map_err(|_| Error::IntegerOutOfRange(what))
    }

    pub(crate) fn bool(&mut self) -> Result<bool> {
        match self.byte()? {
            FALSE => Ok(false),
            TRUE => Ok(true),
            m => Err(Error::UnexpectedType { expected: "bool", marker: m }),
        }
    }

    /// Reads a float, also accepting integers.
    pub(crate) fn float(&mut self) -> Result<f64> {
        match self.peek()? {
            FLOAT32 => {
                self.pos += 1;
                Ok(f32::from_be_bytes(self.be()?) as f64)
            }
            FLOAT64 => {
                self.pos += 1;
                Ok(f64::from_be_bytes(self.be()?))
            }
            _ => self.int().map(|i| i as f64),
        }
    }

    /// Skips one complete value of any type.
    pub(crate) fn skip(&mut self) -> Result<()> {
        self.skip_at(0)
    }

    fn skip_at(&mut self, depth: usize) -> Result<()> {
        let m = self.byte()?;
        // Bytes of data to skip, and nested values to skip after them (a map entry is two).
        let (skip_bytes, children): (usize, usize) = match m {
            0..=POSITIVE_FIXINT_LAST | NEGATIVE_FIXINT..=u8::MAX | NIL | FALSE | TRUE => (0, 0),
            FIXMAP..=FIXMAP_LAST => (0, 2 * (m & FIX_COLLECTION_LEN) as usize),
            FIXARRAY..=FIXARRAY_LAST => (0, (m & FIX_COLLECTION_LEN) as usize),
            FIXSTR..=FIXSTR_LAST => ((m & FIXSTR_LEN) as usize, 0),
            BIN8 | STR8 => (self.len_u8()?, 0),
            BIN16 | STR16 => (self.len_u16()?, 0),
            BIN32 | STR32 => (self.len_u32()?, 0),
            EXT8 => (self.len_u8()? + EXT_TYPE_LEN, 0),
            EXT16 => (self.len_u16()? + EXT_TYPE_LEN, 0),
            EXT32 => (self.len_u32()? + EXT_TYPE_LEN, 0),
            FLOAT32 => (size_of::<f32>(), 0),
            FLOAT64 => (size_of::<f64>(), 0),
            UINT8 | INT8 => (size_of::<u8>(), 0),
            UINT16 | INT16 => (size_of::<u16>(), 0),
            UINT32 | INT32 => (size_of::<u32>(), 0),
            UINT64 | INT64 => (size_of::<u64>(), 0),
            FIXEXT1 => (EXT_TYPE_LEN + 1, 0),
            FIXEXT2 => (EXT_TYPE_LEN + 2, 0),
            FIXEXT4 => (EXT_TYPE_LEN + 4, 0),
            FIXEXT8 => (EXT_TYPE_LEN + 8, 0),
            FIXEXT16 => (EXT_TYPE_LEN + 16, 0),
            ARRAY16 => (0, self.len_u16()?),
            ARRAY32 => (0, self.len_u32()?),
            MAP16 => (0, 2 * self.len_u16()?),
            MAP32 => (0, 2 * self.len_u32()?),
            NEVER_USED => return Err(Error::UnexpectedType { expected: "any value", marker: m }),
        };
        self.take(skip_bytes)?;
        if children > 0 && depth == MAX_SKIP_DEPTH {
            return Err(Error::TooDeep);
        }
        if children > self.remaining() {
            return Err(Error::Truncated);
        }
        for _ in 0..children {
            self.skip_at(depth + 1)?;
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
    fn array_longer_than_the_input_is_truncated() {
        // Five bytes claiming u32::MAX elements: rejected before anyone allocates for them.
        let mut r = Reader::new(&[0xdd, 0xff, 0xff, 0xff, 0xff]);
        assert!(matches!(r.array(0), Err(Error::Truncated)));
        let mut r = Reader::new(&[0xdf, 0xff, 0xff, 0xff, 0xff]);
        assert!(matches!(r.skip(), Err(Error::Truncated)), "same for maps");
        let mut r = Reader::new(&[0x92, 0x01, 0x02]);
        assert_eq!(r.array(0).unwrap(), 2, "a length that fits is fine");
    }

    #[test]
    fn skip_stops_at_hostile_nesting() {
        let deep = |levels: usize| [vec![0x91; levels], vec![0x00]].concat();
        assert!(Reader::new(&deep(MAX_SKIP_DEPTH)).skip().is_ok());
        assert!(matches!(Reader::new(&deep(MAX_SKIP_DEPTH + 1)).skip(), Err(Error::TooDeep)));
        // As deep as a datagram allows, on a small stack: an error, not a stack overflow.
        let input = deep(65_000);
        let result = std::thread::Builder::new()
            .stack_size(64 * 1024)
            .spawn(move || matches!(Reader::new(&input).skip(), Err(Error::TooDeep)))
            .unwrap()
            .join()
            .unwrap();
        assert!(result);
    }

    #[test]
    fn truncated_input_is_an_error() {
        let mut r = Reader::new(&[0xcd, 0x01]);
        assert!(matches!(r.int(), Err(Error::Truncated)));
    }

    /// The first bytes written at each size boundary, from the MessagePack specification.
    #[test]
    fn smallest_encoding_at_every_boundary() {
        let head = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut out = Vec::new();
            f(&mut out);
            out
        };
        for (len, expected) in
            [(15, &[0x9f][..]), (16, &[0xdc, 0x00, 0x10]), (65_535, &[0xdc, 0xff, 0xff]), (65_536, &[0xdd, 0, 1, 0, 0])]
        {
            assert_eq!(head(&|o| write_array_len(o, len).unwrap()), expected, "array of {len}");
        }
        for (len, expected) in [
            (31, &[0xbf][..]),
            (32, &[0xd9, 32]),
            (255, &[0xd9, 255]),
            (256, &[0xda, 0x01, 0x00]),
            (65_536, &[0xdb, 0, 1, 0, 0]),
        ] {
            let s = "x".repeat(len);
            assert_eq!(&head(&|o| write_str(o, &s).unwrap())[..expected.len()], expected, "string of {len}");
        }
        for (len, expected) in
            [(0, &[0xc4, 0][..]), (255, &[0xc4, 255]), (256, &[0xc5, 0x01, 0x00]), (65_536, &[0xc6, 0, 1, 0, 0])]
        {
            let b = vec![0u8; len];
            assert_eq!(&head(&|o| write_bin(o, &b).unwrap())[..expected.len()], expected, "binary of {len}");
        }
        for (v, expected) in [
            (255u64, &[0xcc, 0xff][..]),
            (65_535, &[0xcd, 0xff, 0xff]),
            (65_536, &[0xce, 0, 1, 0, 0]),
            (u32::MAX as u64, &[0xce, 0xff, 0xff, 0xff, 0xff]),
        ] {
            assert_eq!(head(&|o| write_uint(o, v)), expected, "uint {v}");
        }
        assert_eq!(head(&|o| write_bool(o, true)), [0xc3]);
        assert_eq!(head(&|o| write_bool(o, false)), [0xc2]);
    }
}

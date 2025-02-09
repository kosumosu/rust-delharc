use core::num::Wrapping;
use core::slice;
use std::fmt::Write;
use std::io::{self, Read};
use std::path::PathBuf;
use std::borrow::Cow;
use crate::crc::Crc16;
use super::*;

/// Raw identifiers of extra headers.
pub mod ext {
    /// The "Common" header's CRC-16 field will always be reset to 0 in the parsed header data.
    /// This is the necessary condition to verify header's checksum.
    pub const EXT_HEADER_COMMON:      u8 = 0x00;
    pub const EXT_HEADER_FILENAME:    u8 = 0x01;
    pub const EXT_HEADER_PATH:        u8 = 0x02;
    pub const EXT_HEADER_MULTI_DISC:  u8 = 0x39;
    pub const EXT_HEADER_COMMENT:     u8 = 0x3F;
    pub const EXT_HEADER_MSDOS_ATTRS: u8 = 0x40;
    pub const EXT_HEADER_MSDOS_TIME:  u8 = 0x41;
    pub const EXT_HEADER_MSDOS_SIZE:  u8 = 0x42;
    pub const EXT_HEADER_UNIX_PERM:   u8 = 0x50;
    pub const EXT_HEADER_UNIX_UIDGID: u8 = 0x51;
    pub const EXT_HEADER_UNIX_GROUP:  u8 = 0x52;
    pub const EXT_HEADER_UNIX_OWNER:  u8 = 0x53;
    pub const EXT_HEADER_UNIX_TIME:   u8 = 0x54;
    pub const EXT_HEADER_OS9:         u8 = 0xCC;
    pub const EXT_HEADER_EXT_ATTRS:   u8 = 0x7F;
}

use ext::*;
/// An iterator through extra headers, yielding the headers' raw content excluding
/// the next header length field.
pub struct ExtraHeaderIter<'a> {
    data: &'a [u8],
    header_length: u32,
    header_len32: bool
}

impl<'a> Iterator for ExtraHeaderIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let header_length = self.header_length as usize;
        if header_length == 0 {
            return None
        }
        let counter_size = if self.header_len32 { 4 } else { 2 };
        let (res, data) = self.data.split_at(header_length);
        let (res, len) = res.split_at(header_length - counter_size);
        let len = if self.header_len32 {
            read_u32(len).unwrap()
        }
        else {
            read_u16(len).unwrap() as u32
        };
        self.header_length = len;
        self.data = data;
        Some(res)
    }
}

#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
#[repr(packed)]
struct LhaRawBaseHeader {
    compression: [u8;5],
    compressed_size: [u8;4],
    original_size: [u8;4],
    last_modified: [u8;4],
    msdos_attrs: u8,
    lha_level: u8
}

struct Parser<R> {
    rd: R,
    crc: Crc16,
    csum: Wrapping<u8>,
    len: usize
}

impl<R: Read> Parser<R> {
    // NOTE: does not update wrapping sum
    fn read_u8_or_none(&mut self) -> io::Result<Option<u8>> {
        self.rd.by_ref().bytes().next().transpose().map(|mb|
            mb.map(|byte| {
                self.update_checksums_no_wrapping_sum(slice::from_ref(&byte));
                byte
            })
        )
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        let mut byte: u8 = 0;
        self.read_exact(slice::from_mut(&mut byte))?;
        Ok(byte)
    }

    fn read_u16(&mut self) -> io::Result<u16> {
        let mut buf = [0u8;2];
        self.read_exact(&mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }

    fn read_u32(&mut self) -> io::Result<u32> {
        let mut buf = [0u8;4];
        self.read_exact(&mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.rd.read_exact(buf)?;
        self.update_checksums(buf);
        Ok(())
    }

    fn read_limit(&mut self, limit: usize) -> io::Result<Box<[u8]>> {
        let mut buf = Vec::with_capacity(limit);
        self.read_limit_no_checksums(limit, &mut buf)?;
        self.update_checksums(&mut buf);
        Ok(buf.into_boxed_slice())
    }

    fn update_checksums(&mut self, buf: &[u8]) {
        self.update_checksums_no_wrapping_sum(buf);
        self.csum = wrapping_csum(self.csum, buf);
    }

    fn update_checksums_no_wrapping_sum(&mut self, buf: &[u8]) {
        self.len += buf.len();
        self.crc.digest(buf);
    }

    fn read_limit_no_checksums(&mut self, limit: usize, buf: &mut Vec<u8>) -> io::Result<()> {
        if self.rd.by_ref().take(limit as u64).read_to_end(buf)? != limit {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "file is too short"))
        }
        Ok(())
    }
}

impl LhaHeader {
    /// Attempts to parse the LHA header. Returns `Ok(Some(LhaHeader))` on success. Returns `Ok(None)`
    /// if the end of archive marker (a `0` byte) was encountered.
    ///
    /// The method validates all length and checksum fields of the header, but does not parse extra
    /// headers except:
    /// * The ["Common"][EXT_HEADER_COMMON] header for validating the header's CRC-16 checksum.
    /// * The ["MS-DOS Attributes"][EXT_HEADER_MSDOS_ATTRS] header for reading MS-DOS attributes.
    /// * The ["MS-DOS Size"][EXT_HEADER_MSDOS_SIZE] header for reading 64-bit file size.
    ///
    /// All extra data is available as raw bytes and extra headers can be iterated with [LhaHeader::iter_extra].
    ///
    /// Instance methods can be further called on the parsed `LhaHeader` struct to attempt to parse the
    /// name and path of the file or other file's meta-data.
    ///
    /// # Errors
    /// Returns an error from the underlying reading operations or because a malformed header was encountered.
    pub fn read<R: Read>(rd: R) -> io::Result<Option<LhaHeader>> {
        let mut parser = Parser {
            rd, 
            crc: Crc16::default(),
            csum: Wrapping(0),
            len: 0
        };
        let header_len = match parser.read_u8_or_none()? {
            Some(0)|None => return Ok(None),
            Some(len) => len
        };
        let csum = parser.read_u8()?;
        // reset wrapping checksum which should not include the first 2 bytes
        parser.csum = Wrapping(0);

        let mut raw_header = LhaRawBaseHeader::default();
        parser.read_exact(unsafe {
            // safe because LhaRawBaseHeader is packed and contains only byte type members
            struct_slice_mut(&mut raw_header)
        })?;
        if raw_header.lha_level > 3 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown header level"))
        }

        // read filename if level 0 or 1
        let filename = if raw_header.lha_level < 2 {
            let filename_len = parser.read_u8()? as usize;
            if (header_len as usize) < parser.len + filename_len {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "wrong header size"))
            }
            parser.read_limit(filename_len)?
        }
        else {
            Box::new([])
        };

        // file CRC-16
        let file_crc = parser.read_u16()?;

        // OS-TYPE
        let mut os_type = 0;
        if raw_header.lha_level > 0 {
            os_type = parser.read_u8()?;
        }

        // extended area, only 0 and 1 level
        let mut extended_area: Box<[u8]> = Box::new([]);
        if raw_header.lha_level < 2 {
            let mut min_len = parser.len;
            if raw_header.lha_level == 0 {
                min_len -= 2; // no extra headers
            }
            if (header_len as usize) < min_len {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "wrong header size"))
            }
            let mut extended_len = (header_len as usize) - min_len;
            if extended_len != 0 && raw_header.lha_level == 0  {
                // get os_type from level 0 extended area
                extended_len -= 1;
                os_type = parser.read_u8()?;
            }
            if extended_len != 0 {
                extended_area = parser.read_limit(extended_len)?;
            }
        };

        // extra headers
        let mut long_header_len: u32 = 0; // a long header length found in level >= 2
        let mut first_header_len: u32 = 0;
        let mut extra_headers = Vec::new();
        // establish the first extra header length and the long header length
        match raw_header.lha_level {
            1 => {
                first_header_len = parser.read_u16()? as u32;
            }
            2 => {
                long_header_len = u16::from_le_bytes([header_len, csum]) as u32;
                first_header_len = parser.read_u16()? as u32;
            }
            3 => {
                long_header_len = parser.read_u32()?;
                first_header_len = parser.read_u32()?;
                if header_len != 4 || csum != 0 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid header"))
                }
            }
            _ => {}
        }

        // validate level 0 and 1 header checksum
        if raw_header.lha_level < 2 {
            if csum != parser.csum.0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid header level checksum"))
            }
        }
        else if long_header_len < parser.len as u32 + first_header_len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "wrong header size"))
        }

        let mut msdos_attrs = MsDosAttrs::from_bits_retain(raw_header.msdos_attrs as u16);
        let mut original_size = u32::from_le_bytes(raw_header.original_size) as u64;
        let mut compressed_size = u32::from_le_bytes(raw_header.compressed_size) as u64;
        let mut header_crc: Option<u16> = None;
        // read extra headers
        let min_header_len = if raw_header.lha_level == 3 { 5 } else { 3 };
        let mut extra_header_len = first_header_len as usize;
        while extra_header_len != 0 {
            if extra_header_len < min_header_len {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "wrong extra header size"))
            }
            // check long header length (level 2, 3)
            if long_header_len != 0 {
                if (long_header_len as usize) < parser.len + extra_header_len - 2 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "wrong header size"))
                }
            }
            else if compressed_size < (extra_headers.len() + extra_header_len) as u64 {
                // otherwise check skip size (level 1)
                return Err(io::Error::new(io::ErrorKind::InvalidData, "wrong header size"))
            }
            parser.read_limit_no_checksums(extra_header_len, &mut extra_headers)?;
            let start = extra_headers.len() - extra_header_len;
            let header = &mut extra_headers[start..];
            match header {
                // we need to extract the CRC-16 from header and clear it in order to calculate checksum
                [EXT_HEADER_COMMON, data @ ..] => {
                    if header_crc.is_some() {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "double common CRC-16 header"))
                    }
                    if let Some(crc) = data.get_mut(0..2) {
                        header_crc = read_u16(crc);
                        for p in crc.iter_mut() {
                            *p = 0;
                        }
                    }
                }
                [EXT_HEADER_MSDOS_ATTRS, data @ ..]|
                [EXT_HEADER_EXT_ATTRS,   data @ ..] if data.len() >= 2 => {
                    if let Some(attrs) = read_u16(&data[0..2]) {
                        msdos_attrs = MsDosAttrs::from_bits_retain(attrs);
                    }
                }
                [EXT_HEADER_MSDOS_SIZE, data @ ..] if raw_header.lha_level >= 2 && data.len() >= 16 => {
                    match (read_u64(&data[0..8]), read_u64(&data[8..16])) {
                        (Some(compr), Some(orig)) => {
                            compressed_size = compr;
                            original_size = orig;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            parser.update_checksums_no_wrapping_sum(header);
            extra_header_len = if raw_header.lha_level == 3 {
                read_u32(&header[header.len() - 4..]).unwrap() as usize
            }
            else {
                read_u16(&header[header.len() - 2..]).unwrap() as usize
            }
        }

        // validate long header length
        if long_header_len != 0 {
            if long_header_len != parser.len as u32 {
                if raw_header.lha_level == 2 && long_header_len == parser.len as u32 + 1
                {
                    // read padding byte
                    parser.read_u8()?;
                }
                else if raw_header.lha_level == 2 && long_header_len + 2 != parser.len as u32 {
                    // some packers (Osk) don't include self in the header length
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "wrong length of headers"))
                }
            }
        }

        // validate headers CRC
        if let Some(crc) = header_crc {
            if crc != parser.crc.sum16() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "wrong header CRC-16 checksum"))
            }
        }

        // adjust compressed size for level 1
        if raw_header.lha_level == 1 {
            if extra_headers.len() as u64 > compressed_size {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "wrong length of skip size"))
            }
            compressed_size -= extra_headers.len() as u64;
        }

        let compression = raw_header.compression;
        let last_modified = u32::from_le_bytes(raw_header.last_modified);
        let extra_headers = extra_headers.into_boxed_slice();

        Ok(Some(LhaHeader {
            level: raw_header.lha_level,
            compression,
            compressed_size,
            original_size,
            filename,
            os_type,
            msdos_attrs,
            last_modified,
            file_crc,
            extended_area,
            first_header_len,
            extra_headers
        }))
    }

    /// Returns an iterator that will iterate through extra headers, yielding the headers' raw
    /// data, excluding the next header length field.
    ///
    /// # Note
    /// Each iterated raw header will have at least the size of 1 byte containing the header identifier.
    pub fn iter_extra(&self) -> ExtraHeaderIter<'_> {
        ExtraHeaderIter {
            data: &self.extra_headers,
            header_length: self.first_header_len,
            header_len32: self.level == 3
        }
    }
}

fn read_u16(slice: &[u8]) -> Option<u16> {
    match slice {
        &[lo, hi] => Some(u16::from_le_bytes([lo, hi])),
        _ => None
    }
}

pub(super) fn read_u32(slice: &[u8]) -> Option<u32> {
    match slice {
        &[b0, b1, b2, b3] => Some(u32::from_le_bytes([b0, b1, b2, b3])),
        _ => None
    }
}

pub(super) fn read_u64(slice: &[u8]) -> Option<u64> {
    match slice {
        &[b0, b1, b2, b3, b4, b5, b6, b7] => Some(u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7])),
        _ => None
    }
}

fn wrapping_csum(init: Wrapping<u8>, data: &[u8]) -> Wrapping<u8> {
    let sum: Wrapping<u8> = data.iter().copied().map(Wrapping).sum();
    sum + init
}

pub(super) fn split_data_at_nil_or_end(data: &[u8]) -> (&[u8], Option<&[u8]>) {
    match memchr::memchr(0, data) {
        Some(index) => (&data[0..index], Some(&data[index + 1..data.len()])),
        None => (data, None)
    }
}

pub(super) fn parse_pathname(data: &[u8], path: &mut PathBuf) {
    path.reserve(data.len());
    // split by all possible path separators
    for part in data.split(|&c| c == 0xFF || c == b'/' || c == b'\\') {
        match part {
            b"."|b".."|[] => {} // ignore malicious and empty paths
            name => path.push(parse_str_nilterm(name, false, false).as_ref())
        }
    }
}

pub(super) fn parse_str_nilterm(
        data: &[u8], nilterm: bool, ignore_sep: bool
    ) -> Cow<str>
{
    if let Some(index) = data.iter().position(|&c|
            c < 0x20 || c >= 0x7f ||
            (!ignore_sep && std::path::is_separator(c as char))
        )
    {
        let mut out = String::with_capacity(data.len()*3);
        let (head, rest) = data.split_at(index);
        out.push_str(unsafe { // safe because head was validated
            std::str::from_utf8_unchecked(head)
        });
        for byte in rest.iter() {
            match byte {
                0 if nilterm => break,
                0x00..=0x1f|
                0x7f..=0xff => {
                    write!(out, "%{:02x}", byte).unwrap();
                }
                &ch => {
                    let c = ch as char;
                    if !ignore_sep && std::path::is_separator(c) {
                        out.push('_');
                    }
                    else {
                        out.push(c);
                    }
                }
            }
        }
        Cow::Owned(out)
    }
    else {
        unsafe { // safe because data was validated
            Cow::Borrowed(std::str::from_utf8_unchecked(data))
        }
    }
}

/// # Safety
/// This function can be used safely only with packed structs that solely consist of
/// `u8` or array of `u8` primitives.
unsafe fn struct_slice_mut<T: Copy>(obj: &mut T) -> &mut [u8] {
    let len = core::mem::size_of::<T>() / core::mem::size_of::<u8>();
    core::slice::from_raw_parts_mut(obj as *mut T as *mut u8, len)
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{MAIN_SEPARATOR, PathBuf};

    fn parse_filename(data: &[u8]) -> Cow<str> {
        parse_str_nilterm(data, false, false)
    }

   #[test]
    fn split_data_at_nil_or_end_works() {
        assert_eq!((&b"Foo"[..], None), split_data_at_nil_or_end(b"Foo"));
        assert_eq!((&b"Foo"[..], Some(&b"Bar"[..])), split_data_at_nil_or_end(b"Foo\x00Bar"));
        assert_eq!((&[][..], Some(&b"Bar"[..])), split_data_at_nil_or_end(b"\x00Bar"));
    }

   #[test]
    fn path_parser_works() {
        assert_eq!("", parse_filename(b""));
        assert_eq!("Hello World!", parse_filename(b"Hello World!"));
        if std::path::is_separator('/') {
            assert_eq!("_Hello_World_", parse_filename(b"/Hello/World/"));
        }
        if std::path::is_separator('\\') {
            assert_eq!("_Hello_World_", parse_filename(br"\Hello\World\"));
        }
        assert_eq!("Hello%00World%7f", parse_filename(b"Hello\x00World\x7f"));
        assert_eq!("Hello%01World%ff", parse_filename(b"Hello\x01World\xff"));
        assert_eq!("Hello", parse_str_nilterm(b"Hello\x00World\xff", true, false));
        if std::path::is_separator('/') {
            assert_eq!("He_llo", parse_str_nilterm(b"He/llo\x00World\xff", true, false));
            assert_eq!("He/llo", parse_str_nilterm(b"He/llo\x00World\xff", true, true));
            assert_eq!("He/llo%00World%ff", parse_str_nilterm(b"He/llo\x00World\xff", false, true));
            assert_eq!("_Hello%1fWorld%80", parse_filename(b"/Hello\x1fWorld\x80"));
        }
        let mut path = PathBuf::new();
        parse_pathname(b"", &mut path);
        assert!(path.is_relative());
        assert_eq!("", path.to_str().unwrap());
        parse_pathname(b"/", &mut path);
        assert!(path.is_relative());
        assert_eq!("", path.to_str().unwrap());
        parse_pathname(br"\", &mut path);
        assert!(path.is_relative());
        assert_eq!("", path.to_str().unwrap());
        parse_pathname(br".", &mut path);
        assert!(path.is_relative());
        assert_eq!("", path.to_str().unwrap());
        parse_pathname(br"..", &mut path);
        assert!(path.is_relative());
        assert_eq!("", path.to_str().unwrap());
        parse_pathname(br"./..", &mut path);
        assert!(path.is_relative());
        assert_eq!("", path.to_str().unwrap());
        parse_pathname(br".\..", &mut path);
        assert!(path.is_relative());
        assert_eq!("", path.to_str().unwrap());
        parse_pathname(br"/..\./", &mut path);
        assert!(path.is_relative());
        assert_eq!("", path.to_str().unwrap());
        parse_pathname(br"\../.\", &mut path);
        assert!(path.is_relative());
        assert_eq!("", path.to_str().unwrap());
        parse_pathname(br"foo/bar\baz", &mut path);
        assert!(path.is_relative());
        let expect = format!("foo{}bar{}baz", MAIN_SEPARATOR, MAIN_SEPARATOR);
        assert_eq!(expect, path.to_str().unwrap());
        path.clear();
        parse_pathname(br"\foo/bar\baz/", &mut path);
        assert!(path.is_relative());
        let expect = format!("foo{}bar{}baz", MAIN_SEPARATOR, MAIN_SEPARATOR);
        assert_eq!(expect, path.to_str().unwrap());
        path.clear();
        parse_pathname(br"/foo\bar/baz\", &mut path);
        assert!(path.is_relative());
        let expect = format!("foo{}bar{}baz", MAIN_SEPARATOR, MAIN_SEPARATOR);
        assert_eq!(expect, path.to_str().unwrap());
        path.clear();
        parse_pathname(b"foo\xffbar\xffbaz", &mut path);
        assert!(path.is_relative());
        let expect = format!("foo{}bar{}baz", MAIN_SEPARATOR, MAIN_SEPARATOR);
        assert_eq!(expect, path.to_str().unwrap());
        path.clear();
        parse_pathname(b"\xfffoo\xffb\x91ar\xffbaz\xff", &mut path);
        assert!(path.is_relative());
        let expect = format!("foo{}b%91ar{}baz", MAIN_SEPARATOR, MAIN_SEPARATOR);
        assert_eq!(expect, path.to_str().unwrap());
        path.clear();
    }
}
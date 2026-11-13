use std::cmp::min;
use std::error::Error;
use std::io;
use std::str;

use bytebuffer::ByteBuffer;

#[allow(non_snake_case)]
pub fn H(u: u16) -> u8 {
  return (u >> 8) as u8;
}
#[allow(non_snake_case)]
pub fn L(u: u16) -> u8 {
  return u as u8;
}

pub fn make_u16(h: u8, l: u8) -> u16 {
  return ((h as u16) << 8) | (l as u16);
}

pub fn write_str_to_buffer(buffer: &mut [u8], offset: usize, max_len: usize, s: &str) {
  // Never panic on an oversized request: clamp the write to the space the
  // buffer actually has, so a short destination (or a bad offset) just
  // truncates instead of taking down the process.
  let available = buffer.len().saturating_sub(offset);
  let len = min(max_len, s.len()).min(available);
  if len > 0 {
    buffer[offset..offset + len].clone_from_slice(&s.as_bytes()[0..len]);
  }
}

pub fn write_0term_str_to_bytebuffer(bytes: &mut ByteBuffer, s: &str) -> u16 {
  let offset = bytes.get_wpos();
  bytes.write_bytes(s.as_bytes());
  bytes.write_u8(0);
  return offset.min(u16::MAX as usize) as u16;
}

pub fn write_0term_str_or_0_to_bytebuffer(bytes: &mut ByteBuffer, s: Option<&str>) -> u16 {
  if let Some(s) = s {
    write_0term_str_to_bytebuffer(bytes, s)
  } else {
    0
  }
}

pub fn align_wpos(bytes: &mut ByteBuffer, alignment: usize) {
  if alignment == 0 {
    return;
  }
  while (bytes.get_wpos() % alignment) != 0 {
    bytes.write_u8(0);
  }
}

pub fn read_0term_str_from_buffer(buffer: &[u8], offset: usize) -> Result<&str, Box<dyn Error>> {
  if offset >= buffer.len() {
    return Err(Box::new(io::Error::from(io::ErrorKind::UnexpectedEof)));
  }
  let ntpos = match buffer[offset..].iter().position(|c| *c == 0) {
    Some(x) => x,
    None => buffer.len() - offset,
  };
  return str::from_utf8(&buffer[offset..][..ntpos]).map_err(|e| Box::new(e) as Box<dyn Error>);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_h_l_make_u16_roundtrip() {
    for &v in &[0u16, 0xFFFF, 0x1234] {
      assert_eq!(make_u16(H(v), L(v)), v);
    }
  }

  #[test]
  fn test_write_str_to_buffer_exact_fit() {
    let mut buf = [0u8; 5];
    write_str_to_buffer(&mut buf, 0, 5, "hello");
    assert_eq!(&buf, b"hello");
  }

  #[test]
  fn test_write_str_to_buffer_truncation() {
    let mut buf = [0u8; 3];
    write_str_to_buffer(&mut buf, 0, 3, "hello");
    assert_eq!(&buf, b"hel");
  }

  #[test]
  fn test_write_str_to_buffer_empty_string() {
    let mut buf = [0u8; 3];
    write_str_to_buffer(&mut buf, 0, 3, "");
    assert_eq!(&buf, &[0u8; 3]);
  }

  #[test]
  fn test_write_str_to_buffer_offset() {
    let mut buf = [0u8; 5];
    write_str_to_buffer(&mut buf, 2, 3, "abc");
    assert_eq!(&buf, &[0, 0, b'a', b'b', b'c']);
  }

  #[test]
  fn test_write_0term_str_to_bytebuffer_offset_and_trailing_zero() {
    let mut bytes = ByteBuffer::new();
    let offset = write_0term_str_to_bytebuffer(&mut bytes, "hi");
    assert_eq!(offset, 0);
    assert_eq!(bytes.into_vec(), vec![b'h', b'i', 0]);

    let mut bytes = ByteBuffer::new();
    bytes.write_u8(42);
    let offset = write_0term_str_to_bytebuffer(&mut bytes, "x");
    assert_eq!(offset, 1);
    assert_eq!(bytes.into_vec(), vec![42, b'x', 0]);
  }

  #[test]
  fn test_write_0term_str_or_0_to_bytebuffer_some_and_none() {
    let mut bytes = ByteBuffer::new();
    let offset = write_0term_str_or_0_to_bytebuffer(&mut bytes, Some("yo"));
    assert_eq!(offset, 0);
    assert_eq!(bytes.into_vec(), vec![b'y', b'o', 0]);

    let mut bytes = ByteBuffer::new();
    bytes.write_u8(1);
    let offset = write_0term_str_or_0_to_bytebuffer(&mut bytes, None);
    assert_eq!(offset, 0);
    assert_eq!(bytes.into_vec(), vec![1]);
  }

  #[test]
  fn test_align_wpos_various_alignments() {
    let mut bytes = ByteBuffer::new();
    bytes.write_u8(1);
    align_wpos(&mut bytes, 1);
    assert_eq!(bytes.get_wpos(), 1);
    assert_eq!(bytes.into_vec(), vec![1]);

    let mut bytes = ByteBuffer::new();
    bytes.write_u8(1);
    align_wpos(&mut bytes, 2);
    assert_eq!(bytes.get_wpos(), 2);
    assert_eq!(bytes.into_vec(), vec![1, 0]);

    let mut bytes = ByteBuffer::new();
    align_wpos(&mut bytes, 4);
    assert_eq!(bytes.get_wpos(), 0);
    assert_eq!(bytes.into_vec(), vec![]);

    let mut bytes = ByteBuffer::new();
    bytes.write_bytes(&[1, 2, 3]);
    align_wpos(&mut bytes, 4);
    assert_eq!(bytes.get_wpos(), 4);
    assert_eq!(bytes.into_vec(), vec![1, 2, 3, 0]);

    let mut bytes = ByteBuffer::new();
    bytes.write_bytes(&[1, 2, 3, 4, 5]);
    align_wpos(&mut bytes, 8);
    assert_eq!(bytes.get_wpos(), 8);
    assert_eq!(bytes.into_vec(), vec![1, 2, 3, 4, 5, 0, 0, 0]);
  }

  #[test]
  fn test_read_0term_str_from_buffer_valid() {
    let buf = b"hello\0world";
    assert_eq!(read_0term_str_from_buffer(buf, 0).unwrap(), "hello");
    assert_eq!(read_0term_str_from_buffer(buf, 6).unwrap(), "world");
  }

  #[test]
  fn test_read_0term_str_from_buffer_missing_terminator_reads_to_end() {
    let buf = b"abc";
    assert_eq!(read_0term_str_from_buffer(buf, 0).unwrap(), "abc");
  }

  #[test]
  fn test_read_0term_str_from_buffer_empty_string() {
    let buf = b"\0hello";
    assert_eq!(read_0term_str_from_buffer(buf, 0).unwrap(), "");
  }

  #[test]
  fn test_read_0term_str_from_buffer_bad_utf8() {
    let buf: &[u8] = &[0x80, 0x81, 0];
    assert!(read_0term_str_from_buffer(buf, 0).is_err());
  }

  #[test]
  fn test_read_0term_str_from_buffer_offset_past_end() {
    let buf = b"hi";
    assert!(read_0term_str_from_buffer(buf, 5).is_err());
  }
}

#[cfg(test)]
mod robustness_tests {
  use super::*;

  #[test]
  fn write_str_to_buffer_short_destination_does_not_panic() {
    let mut buf = [0u8; 2];
    write_str_to_buffer(&mut buf, 0, 5, "hello");
    assert_eq!(&buf, b"he");
  }

  #[test]
  fn write_str_to_buffer_offset_past_end_does_not_panic() {
    let mut buf = [0u8; 2];
    write_str_to_buffer(&mut buf, 5, 3, "abc");
    assert_eq!(&buf, &[0, 0]);
  }

  #[test]
  fn write_str_to_buffer_zero_max_len_writes_nothing() {
    let mut buf = [0u8; 3];
    write_str_to_buffer(&mut buf, 0, 0, "abc");
    assert_eq!(&buf, &[0, 0, 0]);
  }

  #[test]
  fn align_wpos_zero_alignment_does_not_panic() {
    let mut bytes = ByteBuffer::new();
    bytes.write_u8(7);
    align_wpos(&mut bytes, 0);
    assert_eq!(bytes.get_wpos(), 1);
  }

  #[test]
  fn read_0term_str_offset_at_last_byte() {
    let buf = b"a\0";
    assert_eq!(read_0term_str_from_buffer(buf, 1).unwrap(), "");
  }

  #[test]
  fn make_u16_extremes() {
    assert_eq!(make_u16(0, 0), 0);
    assert_eq!(make_u16(0xFF, 0xFF), 0xFFFF);
  }
}

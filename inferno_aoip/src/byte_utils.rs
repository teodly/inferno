use std::cmp::min;
use std::error::Error;
use std::io;
use std::str;

pub fn H(u: u16) -> u8 {
  return (u >> 8) as u8;
}
pub fn L(u: u16) -> u8 {
  return u as u8;
}

pub fn make_u16(h: u8, l: u8) -> u16 {
  return ((h as u16) << 8) | (l as u16);
}

pub fn write_str_to_buffer(buffer: &mut [u8], offset: usize, max_len: usize, s: &str) {
  let len = min(max_len, s.len());
  buffer[offset..offset + len].clone_from_slice(&s.as_bytes()[0..len]);
}

pub fn read_0term_str_from_buffer(buffer: &[u8], offset: usize) -> Result<&str, Box<dyn Error>> {
  if offset >= buffer.len() {
    return Err(Box::new(io::Error::from(io::ErrorKind::UnexpectedEof)));
  }
  let ntpos = match buffer[offset..].iter().position(|c| *c == 0) {
    Some(x) => x,
    None => buffer.len(),
  };
  return str::from_utf8(&buffer[offset..][..ntpos]).map_err(|e| Box::new(e) as Box<dyn Error>);
}

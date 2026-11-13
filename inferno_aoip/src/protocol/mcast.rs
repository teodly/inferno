use binary_layout::prelude::*;

use crate::byte_utils::*;

pub const HEADER_LENGTH: usize = 32;
pub const INFO_REQUEST_PORT: u16 = 8700;

define_layout!(mcast_packet, BigEndian, {
  start_code: u16,
  total_length: u16,
  seqnum: u16,
  process: u16,
  factory_device_id: [u8; 8],
  vendor: [u8; 8],
  opcode: [u8; 8],
  content: [u8]
});

pub fn make_packet<'a>(
  buffer: &'a mut [u8],
  start_code: u16,
  seqnum: u16,
  process: u16,
  factory_device_id: [u8; 8],
  vendor_str: [u8; 8],
  opcode: [u8; 8],
  content: &[u8],
) -> &'a [u8] {
  let total_len = content.len() + HEADER_LENGTH;
  assert!(total_len <= (1 << 16)); // TODO MAY PANIC
  let buffer = &mut buffer[..total_len]; // TODO MAY PANIC check length before slicing
  let mut view = mcast_packet::View::new(buffer);
  view.start_code_mut().write(start_code);
  view.total_length_mut().write(total_len as u16);
  view.seqnum_mut().write(seqnum);
  view.process_mut().write(process);
  view.factory_device_id_mut().copy_from_slice(&factory_device_id);
  view.vendor_mut().copy_from_slice(&vendor_str);
  view.opcode_mut().copy_from_slice(&opcode);
  view.content_mut().copy_from_slice(&content);
  return view.into_storage();
}

pub struct MulticastMessage {
  pub start_code: u16,
  pub opcode: [u8; 8],
  pub content: Vec<u8>,
}

pub fn make_channel_change_notification(
  channel_indices: impl IntoIterator<Item = usize>,
) -> MulticastMessage {
  // Size the buffer once from the largest channel index instead of
  // resizing repeatedly.
  let indices: Vec<usize> = channel_indices.into_iter().collect();
  let max_byte = indices.iter().map(|ch| ch / 8).max().unwrap_or(0);
  let mut content = vec![0u8; 2 + max_byte + 1];
  let offset = 2;
  for ch in &indices {
    let byte = *ch / 8;
    let bit = *ch % 8;
    content[byte + offset] |= 1 << bit;
  }
  let mask_len = (content.len() - 2).min(u16::MAX as usize) as u16;
  content[0] = H(mask_len);
  content[1] = L(mask_len);
  MulticastMessage { start_code: 0xffff, opcode: [0x07, 0x2a, 1, 2, 0, 0, 0, 0], content }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn make_packet_produces_correct_header_bytes() {
    let mut buf = [0u8; 64];
    let factory_device_id = [1, 2, 3, 4, 5, 6, 7, 8];
    let vendor = [9, 10, 11, 12, 13, 14, 15, 16];
    let opcode = [17, 18, 19, 20, 21, 22, 23, 24];
    let content = [25, 26, 27];
    let packet =
      make_packet(&mut buf, 0x1234, 0x5678, 0x9abc, factory_device_id, vendor, opcode, &content);

    let view = mcast_packet::View::new(packet);
    assert_eq!(view.start_code().read(), 0x1234);
    assert_eq!(view.total_length().read(), (HEADER_LENGTH + content.len()) as u16);
    assert_eq!(view.seqnum().read(), 0x5678);
    assert_eq!(view.process().read(), 0x9abc);
    assert_eq!(view.factory_device_id(), &factory_device_id);
    assert_eq!(view.vendor(), &vendor);
    assert_eq!(view.opcode(), &opcode);
    assert_eq!(view.content(), &content[..]);
  }

  #[test]
  fn make_packet_total_length_equals_header_length_plus_content_len() {
    let mut buf = [0u8; 256];
    for content_len in [0, 1, 10, 100, 200] {
      let content: Vec<u8> = (0..content_len).map(|i| i as u8).collect();
      let packet = make_packet(&mut buf, 0, 0, 0, [0; 8], [0; 8], [0; 8], &content);
      assert_eq!(packet.len(), HEADER_LENGTH + content_len);
      let view = mcast_packet::View::new(packet);
      assert_eq!(view.total_length().read() as usize, HEADER_LENGTH + content_len);
    }
  }

  #[test]
  fn make_channel_change_notification_empty_iterator() {
    let msg = make_channel_change_notification(std::iter::empty::<usize>());
    assert_eq!(msg.content, vec![0, 1, 0]);
  }

  #[test]
  fn make_channel_change_notification_returns_start_code_and_opcode() {
    let msg = make_channel_change_notification([42]);
    assert_eq!(msg.start_code, 0xffff);
    assert_eq!(msg.opcode, [0x07, 0x2a, 1, 2, 0, 0, 0, 0]);
  }
}

#[cfg(test)]
mod robustness_tests {
  use super::*;

  #[test]
  fn notification_channel_index_7_sets_high_bit_of_first_byte() {
    let msg = make_channel_change_notification([7]);
    // content: [mask_len_hi, mask_len_lo, byte0...], mask_len=1
    assert_eq!(msg.content[0], 0);
    assert_eq!(msg.content[1], 1);
    assert_eq!(msg.content[2], 1 << 7);
  }

  #[test]
  fn notification_channel_index_8_resizes_and_sets_bit() {
    let msg = make_channel_change_notification([8]);
    // needs 2 content bytes after the 2-byte length: indices 0 and 1 -> 2 bytes
    assert_eq!(msg.content[0], 0);
    assert_eq!(msg.content[1], 2);
    assert_eq!(msg.content[2], 0);
    assert_eq!(msg.content[3], 1);
  }

  #[test]
  fn notification_multiple_and_duplicate_channels() {
    let msg = make_channel_change_notification([0, 0, 1]);
    assert_eq!(msg.content[0], 0);
    assert_eq!(msg.content[1], 1);
    assert_eq!(msg.content[2], (1 << 0) | (1 << 1));
  }

  #[test]
  fn notification_large_index_does_not_panic() {
    // Should not panic; allocation may be large but 1000 is fine.
    let msg = make_channel_change_notification([1000]);
    assert!(msg.content.len() >= 2);
  }
}

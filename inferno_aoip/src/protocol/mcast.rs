use binary_layout::prelude::*;

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
  view.process_mut().write(0);
  view.factory_device_id_mut().copy_from_slice(&factory_device_id);
  view.vendor_mut().copy_from_slice(&vendor_str);
  view.opcode_mut().copy_from_slice(&opcode);
  view.content_mut().copy_from_slice(&content);
  return view.into_storage();
}

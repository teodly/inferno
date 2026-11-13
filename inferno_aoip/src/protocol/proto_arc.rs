use std::marker::PhantomData;

use binary_serde::BinarySerde;
use bytebuffer::ByteBuffer;
use log::error;

use crate::{byte_utils::make_u16, device_info::DeviceInfo};

use super::req_resp::{Connection, HEADER_LENGTH};

pub const PACKET_SIZE_SOFT_LIMIT: usize = 800;
pub const PORT: u16 = 4440;

/// Entries per page of the receive channel list (0x3000). Controllers reject
/// a receive page holding more entries than this.
pub const RX_CHANNELS_PAGE_SIZE: usize = 16;
/// Entries per page of the transmit channel lists (0x2000, 0x2010).
pub const TX_CHANNELS_PAGE_SIZE: usize = 32;

/// A channel-list entry whose non-zero u16 fields named here are offsets into
/// the packet's string/descriptor area, which follows the entry table.
pub trait PagedEntry: BinarySerde {
  /// Move every non-zero heap offset down by `by` bytes.
  fn shift_heap_offsets(&mut self, by: u16);
}

fn shift_offset(offset: &mut u16, by: u16) {
  if *offset != 0 {
    *offset -= by;
  }
}

pub mod channels_and_flows_count {
  use binary_serde::{binary_serde_bitfield, BinarySerde, BitfieldBitOrder};

  pub const OPCODE: u16 = 0x1000;

  #[derive(Debug, Default, PartialEq, Eq)]
  #[binary_serde_bitfield(order = BitfieldBitOrder::LsbFirst)]
  pub struct Flags2 {
    #[bits(4)]
    pub unknown1_0: u8,
    #[bits(1)]
    pub supports_tx_channel_rename: bool,
    #[bits(1)]
    pub supports_tx_multicast: bool,
    #[bits(2)]
    pub unknown2_0: u8,
  }

  #[derive(Debug, BinarySerde, Default, PartialEq, Eq)]
  pub struct Response {
    pub unknown1_0: u8, // or 5
    pub flags2: Flags2,
    pub tx_channels_count: u16,
    pub rx_channels_count: u16,
    pub unknown2_4: u16, // or 1
    pub max_channels_in_flow: u16,
    pub unknown4_8: u16,
    pub max_tx_flows: u16,
    pub max_rx_flows: u16,
    pub unknown5_total_channels: u16,
    pub unknown6_1: u16,
    pub unknown7_1: u16,
    pub unknown8_0: [u16; 6],
  }
}

pub const GET_DEVICE_NAME_OPCODE: u16 = 0x1002;

pub mod get_device_names {
  use binary_serde::BinarySerde;

  pub const OPCODE: u16 = 0x1003;

  #[derive(Debug, BinarySerde, Default)]
  pub struct ResponseHeader {
    pub unknown1_0: u16,
    pub unknown2_0: u16, // was 0x14
    pub unknown3_0: u16, // was 0x20
    pub board_name_offset: u16,
    pub revision_string_offset: u16,
    pub unknown4_0: u16, // was 0x500
    pub friendly_hostname_offset1: u16,
    pub factory_hostname_offset: u16,
    pub friendly_hostname_offset2: u16,
    pub unknown5_0: [u16; 6], // was [0, 0, 4, 0, 4, 0]
    pub start_code: u16,      // 0x2729
    pub unknown6_0: u16,
    pub unknown_opcode_1102: u16,
    pub unknown7_0: u16,
  }
}

#[derive(Debug, BinarySerde, Default)]
pub struct CommonChannelsDescriptor {
  pub sample_rate: u32,
  pub unknown1_1: u8,
  pub unknown2_1: u8,
  pub bits_per_sample_1: u16,
  pub unknown3_400: u16,
  pub bits_per_sample_2: u16,
  pub bits_per_sample_3: u16,
  pub pcm_type: u16,
}

impl CommonChannelsDescriptor {
  pub fn new(self_info: &DeviceInfo) -> Self {
    Self {
      sample_rate: self_info.sample_rate,
      unknown1_1: 1,
      unknown2_1: 1,
      bits_per_sample_1: self_info.bits_per_sample.into(),
      unknown3_400: 0x400,
      bits_per_sample_2: self_info.bits_per_sample.into(),
      bits_per_sample_3: self_info.bits_per_sample.into(),
      pcm_type: self_info.pcm_type.into(),
    }
  }
}

pub mod get_receive_channels {
  use binary_serde::BinarySerde;

  pub const OPCODE: u16 = 0x3000;

  #[derive(Debug, BinarySerde, Default)]
  pub struct ChannelDescriptor {
    pub channel_id: u16,
    pub unknown1_6: u16,
    pub common_descriptor_offset: u16,
    pub tx_channel_name_offset: u16,
    pub tx_hostname_offset: u16,
    pub friendly_name_offset: u16,
    pub subscription_status: u32, // TODO. 0x01010009 if subscribed currently, 0x00000001 if not found but remembers subscription or in progress
    pub unknown2_0: u32,
  }

  impl super::PagedEntry for ChannelDescriptor {
    fn shift_heap_offsets(&mut self, by: u16) {
      super::shift_offset(&mut self.common_descriptor_offset, by);
      super::shift_offset(&mut self.tx_channel_name_offset, by);
      super::shift_offset(&mut self.tx_hostname_offset, by);
      super::shift_offset(&mut self.friendly_name_offset, by);
    }
  }
}

pub mod get_transmit_channels {
  use binary_serde::BinarySerde;

  pub const OPCODE: u16 = 0x2000;

  #[derive(Debug, BinarySerde, Default)]
  pub struct ChannelDescriptor {
    pub channel_id: u16,
    pub unknown1_7: u16,
    pub common_descriptor_offset: u16,
    pub name_offset: u16,
  }

  impl super::PagedEntry for ChannelDescriptor {
    fn shift_heap_offsets(&mut self, by: u16) {
      super::shift_offset(&mut self.common_descriptor_offset, by);
      super::shift_offset(&mut self.name_offset, by);
    }
  }
}

pub mod get_transmit_channels_friendly_names {
  use binary_serde::BinarySerde;

  pub const OPCODE: u16 = 0x2010;

  #[derive(Debug, BinarySerde, Default)]
  pub struct ChannelDescriptor {
    pub channel_id_1: u16,
    pub channel_id_2: u16,
    pub friendly_name_offset: u16,
  }

  impl super::PagedEntry for ChannelDescriptor {
    fn shift_heap_offsets(&mut self, by: u16) {
      super::shift_offset(&mut self.friendly_name_offset, by);
    }
  }
}

pub mod rename_tx_channels {
  pub const OPCODE: u16 = 0x2013;

  #[derive(Debug, binary_serde::BinarySerde, Default)]
  pub struct SingleChannelRenameRequest {
    pub unknown1_0: u16,
    pub channel_id: u16,
    pub new_name_offset: u16,
  }
}

pub mod rename_rx_channels {
  pub const OPCODE: u16 = 0x3001;

  #[derive(Debug, binary_serde::BinarySerde, Default)]
  pub struct SingleChannelRenameRequest {
    pub channel_id: u16,
    pub new_name_offset: u16,
  }
}

#[derive(Debug, binary_serde::BinarySerde, Default)]
pub struct DestinationSocketDescriptor {
  pub unknown1_8002: u16,
  pub port: u16,
  pub addr: [u8; 4],
}

pub mod query_tx_flows {
  pub const OPCODE: u16 = 0x2200;

  #[derive(Debug, binary_serde::BinarySerde, Default)]
  pub struct NamesDescriptor {
    pub unknown1_a00: u16,
    pub unknown2_1: u16,
    pub remote_hostname_offset: u16,
    pub remote_rx_flow_name_offset: u16,
    pub unknown3_10: u16, // or 0x3c ???
    pub local_tx_flow_name_offset: u16,
    pub unknown4_0: [u8; 8], // in multicast flows first 4B are latency_ns
  }

  #[derive(Debug, binary_serde::BinarySerde, Default)]
  pub struct FlowDescriptorHeader {
    pub flow_id: u16,
    pub flow_type: u16, // 0x11 for unicast, 2 for multicast
    pub sample_rate: u32,
    pub unknown1_0: u16,
    pub bits_per_sample: u16,
    pub unknown2_1: u16,
    pub channels_count: u16,
    pub receiver_socket_descriptor_offset: u16,
  }

  #[derive(Debug, binary_serde::BinarySerde, Default)]
  pub struct FlowDescriptorFooter {
    pub names_descriptor_offset: u16,
  }
}

pub mod create_multicast_tx_flow {
  pub const OPCODE: u16 = 0x2201;

  #[derive(Debug, binary_serde::BinarySerde, Default)]
  pub struct FlowDescriptorHeader {
    pub flow_id: u16,
    pub flow_type: u16,
    pub unknown1_0: [u8; 10],
    pub channels_count: u16,
  }

  #[derive(Debug, binary_serde::BinarySerde, Default)]
  pub struct FlowDescriptorFooter {
    pub mostly_zeros_offset: u16,
  }

  #[derive(Debug, binary_serde::BinarySerde, Default)]
  pub struct MostlyZeros {
    pub unknown1_a00: u16,
    pub unknown2_0: [u8; 14],
    pub unknown3_1: u16,
    pub unknown4_0: u16,
  }

  /// Parses the flow descriptor at `descr_offset` (relative to the packet start, as
  /// listed in the request) out of the request content: the header and its channel
  /// ids (0 = empty slot). None if any part lies outside the content.
  pub fn parse_descriptor(
    content: &[u8],
    descr_offset: usize,
  ) -> Option<(FlowDescriptorHeader, Vec<u16>)> {
    use binary_serde::BinarySerde;
    let start = descr_offset.checked_sub(super::HEADER_LENGTH)?;
    let header_end = start.checked_add(FlowDescriptorHeader::SERIALIZED_SIZE)?;
    let header = FlowDescriptorHeader::binary_deserialize(
      content.get(start..header_end)?,
      binary_serde::Endianness::Big,
    )
    .ok()?;
    let after_header = &content[header_end..];
    let channels_len = header.channels_count as usize * 2;
    if after_header.len() < channels_len + FlowDescriptorFooter::SERIALIZED_SIZE {
      return None;
    }
    let ids =
      after_header[..channels_len].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    Some((header, ids))
  }
}

pub mod delete_multicast_tx_flow {
  pub const OPCODE: u16 = 0x2202;

  /// Flow ids to delete: `u16 count, u16 ?, count x u16 flow id`. None if the content
  /// is shorter than its 4-byte header; a count larger than the ids present is clamped.
  pub fn parse_flow_ids(content: &[u8]) -> Option<Vec<u16>> {
    if content.len() < 4 {
      return None;
    }
    let count = u16::from_be_bytes([content[0], content[1]]) as usize;
    Some(content[4..].chunks_exact(2).take(count).map(|c| u16::from_be_bytes([c[0], c[1]])).collect())
  }
}

/// Remove subscriptions (used by network-audio-controller's `subscription remove`).
pub mod remove_rx_subscriptions {
  pub const OPCODE: u16 = 0x3014;

  /// Receive channel ids: `u16 count, count x u32 channel id`. None if the content is
  /// shorter than its header; a count larger than the ids present is clamped.
  pub fn parse_channel_ids(content: &[u8]) -> Option<Vec<u32>> {
    if content.len() < 2 {
      return None;
    }
    let count = u16::from_be_bytes([content[0], content[1]]) as usize;
    Some(
      content[2..]
        .chunks_exact(4)
        .take(count)
        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect(),
    )
  }

  /// Splits the requested channel ids (1-based) into local channel indices to
  /// unsubscribe and ids that name no receive channel. A bulk remove names
  /// every channel; acting on the first id only left the rest subscribed (U1).
  pub fn local_channel_indices(ids: &[u32], rx_channels: usize) -> (Vec<usize>, Vec<u32>) {
    let mut valid = Vec::with_capacity(ids.len());
    let mut invalid = Vec::new();
    for &id in ids {
      if id == 0 || id as usize > rx_channels {
        invalid.push(id);
      } else {
        valid.push((id - 1) as usize);
      }
    }
    (valid, invalid)
  }
}

pub mod query_rx_flows {
  pub const OPCODE: u16 = 0x3200;

  #[derive(Debug, binary_serde::BinarySerde, Default)]
  pub struct Descriptor2 {
    pub unknown1_9: u16,
    pub unknown2_1: u16,
    pub unknown3_800: u16,
    pub unknown4_0: u16,
    pub latency_ns: u32,
    pub unknown5_0: u32,
  }

  #[derive(Debug, binary_serde::BinarySerde, Default)]
  pub struct FlowDescriptorHeader {
    pub flow_id: u16,
    pub unknown1_1: u16,
    pub sample_rate: u32,
    pub unknown2_0: u16,
    pub bits_per_sample: u16,
    pub unknown3_1: u16,
    pub channels_count: u16,
    pub words_per_bitmask: u16,
    pub receiver_socket_descriptor_offset: u16,
  }

  #[derive(Debug, binary_serde::BinarySerde, Default)]
  pub struct FlowDescriptorFooter {
    pub descriptor2_offset: u16,
  }
}

pub mod set_channels_subscriptions {
  pub const OPCODE: u16 = 0x3010;

  #[derive(Debug, binary_serde::BinarySerde, Default)]
  pub struct SingleChannelSubscriptionRequest {
    pub local_channel_id: u16,
    pub tx_channel_name_offset: u16,
    pub tx_hostname_offset: u16,
  }
}

pub fn serialize_items<InItem, OutItem>(
  space_items: u8,
  source: impl IntoIterator<Item = InItem>,
  mut transform: impl FnMut(InItem, &mut ByteBuffer) -> Option<OutItem>,
) -> (bool, Vec<u8>)
where
  OutItem: BinarySerde,
{
  let mut bytes = ByteBuffer::new();
  bytes.write_bytes(&[0u8; HEADER_LENGTH]);
  bytes.write_u8(space_items);
  bytes.write_u8(0);
  if space_items == 0 {
    return (false, bytes.as_bytes()[HEADER_LENGTH..].into());
  }
  let space_items: usize = space_items.into();
  bytes.write_bytes(&vec![0u8; space_items * OutItem::SERIALIZED_SIZE]);

  let source = source.into_iter();
  let mut item_pos = 2 + HEADER_LENGTH;
  let mut actual_items = 0;
  let mut have_more = false;

  let mut tmp_buffer = vec![0u8; OutItem::SERIALIZED_SIZE];
  for in_item in source {
    if actual_items >= space_items {
      have_more = true;
      break;
    }
    let out_item = if let Some(item) = transform(in_item, &mut bytes) {
      item
    } else {
      continue;
    };
    out_item.binary_serialize(&mut tmp_buffer, binary_serde::Endianness::Big);
    let prev_pos = bytes.get_wpos();
    bytes.set_wpos(item_pos);
    bytes.write_bytes(&tmp_buffer);
    bytes.set_wpos(prev_pos);
    item_pos += OutItem::SERIALIZED_SIZE;
    if prev_pos >= PACKET_SIZE_SOFT_LIMIT {
      have_more = true;
      break;
    }
    actual_items += 1;
  }
  bytes.set_wpos(1 + HEADER_LENGTH);
  bytes.write_u8(actual_items.try_into().unwrap());
  (have_more, bytes.as_bytes()[HEADER_LENGTH..].into())
}

/// Like `serialize_items`, but the entry table is packed: it holds exactly
/// the entries sent, with the string/descriptor area right after it.
/// `serialize_items` reserves `space_items` slots up front, so a short page
/// (the last page of a list, or one cut by `PACKET_SIZE_SOFT_LIMIT`) carries
/// zeroed slots between the table and the strings, which controllers reject
/// as a malformed page.
pub fn serialize_page<InItem, OutItem>(
  space_items: u8,
  source: impl IntoIterator<Item = InItem>,
  mut transform: impl FnMut(InItem, &mut ByteBuffer) -> Option<OutItem>,
) -> (bool, Vec<u8>)
where
  OutItem: PagedEntry,
{
  let mut bytes = ByteBuffer::new();
  bytes.write_bytes(&[0u8; HEADER_LENGTH]);
  bytes.write_u8(space_items);
  bytes.write_u8(0);
  if space_items == 0 {
    return (false, bytes.as_bytes()[HEADER_LENGTH..].into());
  }
  let space_items: usize = space_items.into();
  // one past the page tells whether more entries follow it
  let mut source: Vec<InItem> = source.into_iter().take(space_items + 1).collect();
  let mut have_more = source.len() > space_items;
  source.truncate(space_items);
  let table_start = 2 + HEADER_LENGTH;
  let reserved = source.len();
  bytes.write_bytes(&vec![0u8; reserved * OutItem::SERIALIZED_SIZE]);

  let mut items = Vec::with_capacity(reserved);
  // End of the data the emitted entries own. The transform writes an
  // entry's strings and descriptors before the size check, so an entry
  // that crosses the limit has already appended its data: the page is cut
  // where that data began, or it would carry bytes no entry points to
  // (and run past the soft limit by up to one entry's data). Safe because
  // the rejected entry is never the first, so data it shares with earlier
  // entries (the common descriptor) was written before it.
  let mut heap_end = bytes.get_wpos();
  for in_item in source {
    let entry_start = bytes.get_wpos();
    let out_item = match transform(in_item, &mut bytes) {
      Some(item) => item,
      None => continue,
    };
    // as in serialize_items, the entry that crosses the limit goes on the next page
    if bytes.get_wpos() >= PACKET_SIZE_SOFT_LIMIT && !items.is_empty() {
      have_more = true;
      heap_end = entry_start;
      break;
    }
    items.push(out_item);
    heap_end = bytes.get_wpos();
  }

  let gap = (reserved - items.len()) * OutItem::SERIALIZED_SIZE;
  let table_end = table_start + reserved * OutItem::SERIALIZED_SIZE;
  let raw = bytes.as_bytes();
  let mut out = Vec::with_capacity(raw.len() - gap - HEADER_LENGTH);
  // Both bytes carry the number of entries in this page. A controller reads
  // a receive page whose first byte is larger than its entry count as
  // malformed; for a full page, and for every single-page list, this is the
  // value inferno always sent.
  out.push(items.len().try_into().unwrap());
  out.push(items.len().try_into().unwrap());
  let mut tmp_buffer = vec![0u8; OutItem::SERIALIZED_SIZE];
  for mut item in items {
    item.shift_heap_offsets(gap.try_into().unwrap());
    item.binary_serialize(&mut tmp_buffer, binary_serde::Endianness::Big);
    out.extend_from_slice(&tmp_buffer);
  }
  out.extend_from_slice(&raw[table_end..heap_end]);
  (have_more, out)
}

pub fn extract_start_index(request_payload: &[u8]) -> Option<usize> {
  if request_payload.len() < 4 || (request_payload[2] | request_payload[3]) == 0 {
    error!("got invalid paginate request, payload: {request_payload:?}");
    return None;
  }
  Some((make_u16(request_payload[2], request_payload[3]) - 1).into())
}

pub fn paginate_make_response<InItem, OutItem>(
  connection: &mut Connection,
  request_payload: &[u8],
  space_items: u8,
  source: impl IntoIterator<Item = InItem>,
  transform: impl FnMut(InItem, &mut ByteBuffer) -> Option<OutItem>,
) -> (u16, Vec<u8>)
where
  OutItem: BinarySerde,
{
  let start_index = match extract_start_index(request_payload) {
    Some(v) => v,
    None => {
      error!("unable to extract start index from request payload {}", hex::encode(request_payload));
      return (0xFFFF /* TODO */, vec![]);
    }
  };
  let (have_more, bytes) = serialize_items(space_items, source.into_iter().skip(start_index), transform);
  let code = if have_more { 0x8112 } else { 1 };
  (code, bytes)
}

/// Responds with one page of a channel list (packed, see `serialize_page`).
pub async fn paginate_respond<InItem, OutItem>(
  connection: &mut Connection,
  request_payload: &[u8],
  space_items: u8,
  source: impl IntoIterator<Item = InItem>,
  transform: impl FnMut(InItem, &mut ByteBuffer) -> Option<OutItem>,
) where
  OutItem: PagedEntry,
{
  let start_index = match extract_start_index(request_payload) {
    Some(v) => v,
    None => {
      error!("unable to extract start index from request payload {}", hex::encode(request_payload));
      connection.respond_with_code(0xFFFF /* TODO */, &[]).await;
      return;
    }
  };
  let (have_more, bytes) = serialize_page(space_items, source.into_iter().skip(start_index), transform);
  let code = if have_more { 0x8112 } else { 1 };
  connection.respond_with_code(code, &bytes).await;
}

pub struct ItemsInPacketIterator<'a, T> {
  items_bytes: &'a [u8],
  item_start: usize,
  _t: PhantomData<T>,
}

impl<'a, T: BinarySerde> Iterator for ItemsInPacketIterator<'a, T> {
  type Item = T;
  fn next(&mut self) -> Option<Self::Item> {
    loop {
      let item_start = self.item_start;
      let item_end = item_start + T::SERIALIZED_SIZE;
      self.item_start = item_end;
      if item_end > self.items_bytes.len() {
        return None;
      }
      match T::binary_deserialize(&self.items_bytes[item_start..item_end], binary_serde::Endianness::Big)
      {
        Ok(item) => {
          return Some(item);
        }
        Err(e) => {
          error!(
            "unable to deserialize item in incoming packet: {e:?}, item: {}, all items: {}",
            hex::encode(&self.items_bytes[item_start..item_end]),
            hex::encode(&self.items_bytes)
          );
        }
      }
    }
  }
}

/// Iterates the items of a request payload laid out as `u8, u8 count, count x T`.
/// The count comes from the network: it is clamped to the items actually present,
/// and a payload shorter than its 2-byte header yields no items.
pub fn deserialize_items<'a, T: BinarySerde>(payload: &'a [u8]) -> ItemsInPacketIterator<'a, T> {
  let items_bytes = payload.get(2..).unwrap_or(&[]);
  let num_items: usize = (*payload.get(1).unwrap_or(&0)).into();
  let num_items = num_items.min(items_bytes.len() / T::SERIALIZED_SIZE);
  ItemsInPacketIterator::<'a, T> {
    items_bytes: &items_bytes[..num_items * T::SERIALIZED_SIZE],
    item_start: 0,
    _t: Default::default(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use binary_serde::recursive_array::RecursiveArray;
  use binary_serde::BinarySerde;

  #[test]
  fn flags2_bitfield_roundtrip() {
    let original = channels_and_flows_count::Flags2 {
      unknown1_0: 0x0A,
      supports_tx_channel_rename: true,
      supports_tx_multicast: false,
      unknown2_0: 0x03,
    };
    let bytes = original.binary_serialize_to_array(binary_serde::Endianness::Big);
    let deserialized = channels_and_flows_count::Flags2::binary_deserialize(
      bytes.as_slice(),
      binary_serde::Endianness::Big,
    )
    .unwrap();
    assert_eq!(original.unknown1_0, deserialized.unknown1_0);
    assert_eq!(original.supports_tx_channel_rename, deserialized.supports_tx_channel_rename);
    assert_eq!(original.supports_tx_multicast, deserialized.supports_tx_multicast);
    assert_eq!(original.unknown2_0, deserialized.unknown2_0);
  }

  #[test]
  fn channels_and_flows_count_response_roundtrip() {
    let original = channels_and_flows_count::Response {
      unknown1_0: 5,
      flags2: channels_and_flows_count::Flags2 {
        unknown1_0: 0,
        supports_tx_channel_rename: true,
        supports_tx_multicast: true,
        unknown2_0: 0,
      },
      tx_channels_count: 8,
      rx_channels_count: 8,
      unknown2_4: 1,
      max_channels_in_flow: 8,
      unknown4_8: 0,
      max_tx_flows: 4,
      max_rx_flows: 4,
      unknown5_total_channels: 16,
      unknown6_1: 1,
      unknown7_1: 1,
      unknown8_0: [0; 6],
    };
    let bytes = original.binary_serialize_to_array(binary_serde::Endianness::Big);
    let deserialized = channels_and_flows_count::Response::binary_deserialize(
      bytes.as_slice(),
      binary_serde::Endianness::Big,
    )
    .unwrap();
    assert_eq!(original, deserialized);
  }

  #[test]
  fn common_channels_descriptor_new_matches_device_info() {
    let device = DeviceInfo {
      ip_address: std::net::Ipv4Addr::new(192, 168, 1, 1),
      netmask: std::net::Ipv4Addr::new(255, 255, 255, 0),
      gateway: std::net::Ipv4Addr::new(192, 168, 1, 1),
      mac_address: netdev::mac::MacAddr::from_hex_format("00:11:22:33:44:55"),
      link_speed: 1000,
      board_name: "TestBoard".to_string(),
      manufacturer: "TestMfg".to_string(),
      model_name: "TestModel".to_string(),
      model_number: "123".to_string(),
      factory_device_id: [1, 2, 3, 4, 5, 6, 7, 8],
      process_id: 1,
      vendor_string: "TestVendor".to_string(),
      friendly_hostname: "test".to_string(),
      factory_hostname: "factory".to_string(),
      rx_channels: vec![],
      tx_channels: vec![],
      bits_per_sample: 24,
      pcm_type: 0x0e,
      latency_ns: 5000000,
      sample_rate: 48000,
      arc_port: 4440,
      cmc_port: 8800,
      flows_control_port: 4455,
      info_request_port: 8700,
      product_version: None,
      name_request_path: None,
      latency_request_path: None,
      sample_rate_request_path: None,
      announced_sample_rate: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
      announced_latency_ns: Default::default(),
      state_dir: None,
    };
    let desc = CommonChannelsDescriptor::new(&device);
    assert_eq!(desc.sample_rate, 48000);
    assert_eq!(desc.bits_per_sample_1, 24);
    assert_eq!(desc.bits_per_sample_2, 24);
    assert_eq!(desc.bits_per_sample_3, 24);
    assert_eq!(desc.pcm_type, 0x0e);
    assert_eq!(desc.unknown1_1, 1);
    assert_eq!(desc.unknown2_1, 1);
    assert_eq!(desc.unknown3_400, 0x400);
  }

  #[test]
  fn destination_socket_descriptor_roundtrip() {
    let original =
      DestinationSocketDescriptor { unknown1_8002: 0x8002, port: 5004, addr: [192, 168, 1, 100] };
    let bytes = original.binary_serialize_to_array(binary_serde::Endianness::Big);
    let deserialized =
      DestinationSocketDescriptor::binary_deserialize(bytes.as_slice(), binary_serde::Endianness::Big)
        .unwrap();
    assert_eq!(original.unknown1_8002, deserialized.unknown1_8002);
    assert_eq!(original.port, deserialized.port);
    assert_eq!(original.addr, deserialized.addr);
  }

  #[test]
  fn get_receive_channels_channel_descriptor_roundtrip() {
    let original = get_receive_channels::ChannelDescriptor {
      channel_id: 5,
      unknown1_6: 6,
      common_descriptor_offset: 10,
      tx_channel_name_offset: 20,
      tx_hostname_offset: 30,
      friendly_name_offset: 40,
      subscription_status: 0x01010009,
      unknown2_0: 0,
    };
    let bytes = original.binary_serialize_to_array(binary_serde::Endianness::Big);
    let deserialized = get_receive_channels::ChannelDescriptor::binary_deserialize(
      bytes.as_slice(),
      binary_serde::Endianness::Big,
    )
    .unwrap();
    assert_eq!(original.channel_id, deserialized.channel_id);
    assert_eq!(original.subscription_status, deserialized.subscription_status);
  }

  #[test]
  fn get_transmit_channels_channel_descriptor_roundtrip() {
    let original = get_transmit_channels::ChannelDescriptor {
      channel_id: 3,
      unknown1_7: 7,
      common_descriptor_offset: 12,
      name_offset: 24,
    };
    let bytes = original.binary_serialize_to_array(binary_serde::Endianness::Big);
    let deserialized = get_transmit_channels::ChannelDescriptor::binary_deserialize(
      bytes.as_slice(),
      binary_serde::Endianness::Big,
    )
    .unwrap();
    assert_eq!(original.channel_id, deserialized.channel_id);
    assert_eq!(original.name_offset, deserialized.name_offset);
  }

  #[test]
  fn rename_tx_channels_request_roundtrip() {
    let original = rename_tx_channels::SingleChannelRenameRequest {
      unknown1_0: 0,
      channel_id: 7,
      new_name_offset: 42,
    };
    let bytes = original.binary_serialize_to_array(binary_serde::Endianness::Big);
    let deserialized = rename_tx_channels::SingleChannelRenameRequest::binary_deserialize(
      bytes.as_slice(),
      binary_serde::Endianness::Big,
    )
    .unwrap();
    assert_eq!(original.channel_id, deserialized.channel_id);
    assert_eq!(original.new_name_offset, deserialized.new_name_offset);
  }

  #[test]
  fn query_tx_flows_flow_descriptor_header_roundtrip() {
    let original = query_tx_flows::FlowDescriptorHeader {
      flow_id: 1,
      flow_type: 0x11,
      sample_rate: 48000,
      unknown1_0: 0,
      bits_per_sample: 24,
      unknown2_1: 1,
      channels_count: 2,
      receiver_socket_descriptor_offset: 100,
    };
    let bytes = original.binary_serialize_to_array(binary_serde::Endianness::Big);
    let deserialized = query_tx_flows::FlowDescriptorHeader::binary_deserialize(
      bytes.as_slice(),
      binary_serde::Endianness::Big,
    )
    .unwrap();
    assert_eq!(original.flow_id, deserialized.flow_id);
    assert_eq!(original.flow_type, deserialized.flow_type);
    assert_eq!(original.sample_rate, deserialized.sample_rate);
    assert_eq!(original.channels_count, deserialized.channels_count);
  }

  #[test]
  fn query_rx_flows_descriptor2_roundtrip() {
    let original = query_rx_flows::Descriptor2 {
      unknown1_9: 9,
      unknown2_1: 1,
      unknown3_800: 0x800,
      unknown4_0: 0,
      latency_ns: 5000000,
      unknown5_0: 0,
    };
    let bytes = original.binary_serialize_to_array(binary_serde::Endianness::Big);
    let deserialized =
      query_rx_flows::Descriptor2::binary_deserialize(bytes.as_slice(), binary_serde::Endianness::Big)
        .unwrap();
    assert_eq!(original.latency_ns, deserialized.latency_ns);
    assert_eq!(original.unknown3_800, deserialized.unknown3_800);
  }

  #[test]
  fn extract_start_index_valid() {
    assert_eq!(extract_start_index(&[0, 0, 0, 5]), Some(4));
  }

  #[test]
  fn extract_start_index_too_short() {
    assert_eq!(extract_start_index(&[0, 0, 0]), None);
  }

  #[test]
  fn extract_start_index_zero_value() {
    assert_eq!(extract_start_index(&[0, 0, 0, 0]), None);
  }

  #[test]
  fn deserialize_items_empty() {
    let items: Vec<get_receive_channels::ChannelDescriptor> = deserialize_items(&[0, 0]).collect();
    assert!(items.is_empty());
  }

  #[test]
  fn remove_rx_subscriptions_parse() {
    use remove_rx_subscriptions::parse_channel_ids;
    // netaudio, two channels: u16 count, then u32 ids
    assert_eq!(parse_channel_ids(&[0, 2, 0, 0, 0, 1, 0, 0, 0, 2]), Some(vec![1, 2]));
    // the single-channel example from the original handler's comment
    assert_eq!(parse_channel_ids(&[0, 1, 0, 0, 0, 2]), Some(vec![2]));
    assert_eq!(parse_channel_ids(&[]), None);
    assert_eq!(parse_channel_ids(&[0]), None);
    // count larger than the ids present (the old handler read content[4..6] unchecked)
    assert_eq!(parse_channel_ids(&[0xff, 0xff, 0, 0]), Some(vec![]));
    assert_eq!(parse_channel_ids(&[0, 3, 0, 0, 0, 7, 0]), Some(vec![7]));
  }

  #[test]
  fn bulk_unsubscribe_takes_every_channel() {
    use remove_rx_subscriptions::{local_channel_indices, parse_channel_ids};
    // netaudio's bulk remove of channels 1-3 on a 32-channel receiver
    let ids = parse_channel_ids(&[0, 3, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]).unwrap();
    assert_eq!(local_channel_indices(&ids, 32), (vec![0, 1, 2], vec![]));
    // ids outside the device are reported, the rest still handled
    assert_eq!(local_channel_indices(&[0, 2, 33], 32), (vec![1], vec![0, 33]));
  }

  #[test]
  fn delete_multicast_tx_flow_parse() {
    use delete_multicast_tx_flow::parse_flow_ids;
    assert_eq!(parse_flow_ids(&[0, 2, 0, 0, 0, 1, 0, 5]), Some(vec![1, 5]));
    for len in 0..4 {
      assert_eq!(parse_flow_ids(&[0, 2, 0, 0][..len]), None, "len {len}");
    }
    assert_eq!(parse_flow_ids(&[0xff, 0xff, 0, 0, 0, 9, 1]), Some(vec![9]));
  }

  #[test]
  fn create_multicast_tx_flow_parse() {
    use create_multicast_tx_flow::*;
    // content: u16 ?, u16 count=1, u16 descriptor offset (packet-relative), then the descriptor
    let mut content = vec![0u8, 1, 0, (HEADER_LENGTH + 6) as u8];
    content.extend_from_slice(&[0, 0]);
    let header =
      FlowDescriptorHeader { flow_id: 3, flow_type: 2, unknown1_0: [0; 10], channels_count: 2 };
    content.extend_from_slice(header.binary_serialize_to_array(binary_serde::Endianness::Big).as_slice());
    content.extend_from_slice(&[0, 1, 0, 4]); // channel ids 1, 4
    content.extend_from_slice(&[0, 0]); // footer
    let (h, ids) = parse_descriptor(&content, HEADER_LENGTH + 6).unwrap();
    assert_eq!((h.flow_id, h.flow_type, h.channels_count), (3, 2, 2));
    assert_eq!(ids, vec![1, 4]);
    // offsets below the header length used to underflow `descr_offset - HEADER_LENGTH`
    for offset in 0..HEADER_LENGTH {
      assert!(parse_descriptor(&content, offset).is_none(), "offset {offset}");
    }
    assert!(parse_descriptor(&content, usize::MAX).is_none());
    // every truncation is rejected; the old length check compared the channel *count*
    // with the bytes left, so a list of 2-byte ids could run past the end
    for len in 0..content.len() {
      assert!(parse_descriptor(&content[..len], HEADER_LENGTH + 6).is_none(), "len {len}");
    }
    let mut many = content.clone();
    let count_pos = 6 + FlowDescriptorHeader::SERIALIZED_SIZE - 2;
    many[count_pos..count_pos + 2].copy_from_slice(&3u16.to_be_bytes());
    assert!(parse_descriptor(&many, HEADER_LENGTH + 6).is_none());
  }

  #[test]
  fn deserialize_items_shorter_than_header() {
    for payload in [&[][..], &[0][..], &[0, 5][..]] {
      let items: Vec<u16> = deserialize_items(payload).collect();
      assert!(items.is_empty(), "payload {payload:?}");
    }
  }

  #[test]
  fn deserialize_items_count_larger_than_payload() {
    // count says 5 items of 6 bytes, but only 5 bytes follow the header:
    // the old code took the count from the whole payload (7 / 6 = 1) and sliced past the end
    let payload = [0, 5, 1, 2, 3, 4, 5];
    let items: Vec<set_channels_subscriptions::SingleChannelSubscriptionRequest> =
      deserialize_items(&payload).collect();
    assert!(items.is_empty());
  }

  #[test]
  fn deserialize_items_clamps_to_present_items() {
    let payload = [0, 200, 0, 1, 0, 2, 0, 3];
    let items: Vec<u16> = deserialize_items(&payload).collect();
    assert_eq!(items, vec![1, 2, 3]);
  }

  #[test]
  fn deserialize_items_never_panics_on_short_payloads() {
    for len in 0..64 {
      for count in [0u8, 1, 2, 7, 255] {
        let mut payload = vec![0xAAu8; len];
        if len > 1 {
          payload[1] = count;
        }
        let _: Vec<set_channels_subscriptions::SingleChannelSubscriptionRequest> =
          deserialize_items(&payload).collect();
        let _: Vec<u16> = deserialize_items(&payload).collect();
      }
    }
  }

  // --- U13: packed channel-list pages -------------------------------------

  fn tx_page(channels: usize, start_index: usize) -> (bool, Vec<u8>) {
    use crate::byte_utils::write_0term_str_to_bytebuffer;
    let names: Vec<String> = (1..=channels).map(|i| format!("{i:02}")).collect();
    let mut descriptor_offset = 0u16;
    serialize_page(
      channels.min(TX_CHANNELS_PAGE_SIZE).try_into().unwrap(),
      names.iter().enumerate().skip(start_index),
      |(index, name), bytes| {
        if descriptor_offset == 0 {
          descriptor_offset = bytes.get_wpos().try_into().unwrap();
          bytes.write_u32(48000);
        }
        Some(get_transmit_channels::ChannelDescriptor {
          channel_id: (index + 1).try_into().unwrap(),
          unknown1_7: 7,
          common_descriptor_offset: descriptor_offset,
          name_offset: write_0term_str_to_bytebuffer(bytes, name),
        })
      },
    )
  }

  // every offset in an entry must point at what the transform wrote there,
  // and the string area must start right after the last entry
  fn check_packed(body: &[u8]) {
    let size = get_transmit_channels::ChannelDescriptor::SERIALIZED_SIZE;
    let count = body[1] as usize;
    let table_end = HEADER_LENGTH + 2 + count * size;
    for i in 0..count {
      let e = 2 + i * size;
      let id = u16::from_be_bytes([body[e], body[e + 1]]);
      let descr = u16::from_be_bytes([body[e + 4], body[e + 5]]) as usize;
      let name = u16::from_be_bytes([body[e + 6], body[e + 7]]) as usize;
      assert_eq!(descr, table_end, "descriptor follows the table");
      let at = name - HEADER_LENGTH;
      let end = at + body[at..].iter().position(|&b| b == 0).unwrap();
      assert_eq!(std::str::from_utf8(&body[at..end]).unwrap(), format!("{id:02}"));
    }
  }

  #[test]
  fn short_last_page_is_packed() {
    // 33 channels: page 2 holds one entry of a 32-entry page
    let (more, body) = tx_page(33, 32);
    assert!(!more);
    assert_eq!(body[0], 1, "first byte is the entry count");
    assert_eq!(body[1], 1);
    check_packed(&body);
    assert_eq!(body.len(), 2 + 8 + 4 + 3, "no reserved slots left in the packet");
  }

  #[test]
  fn full_pages_report_more() {
    let (more, body) = tx_page(64, 0);
    assert!(more);
    assert_eq!(body[1], 32);
    check_packed(&body);
    let (more, body) = tx_page(64, 32);
    assert!(!more);
    assert_eq!(body[1], 32);
    check_packed(&body);
  }

  #[test]
  fn soft_limit_cut_page_is_packed() {
    use crate::byte_utils::write_0term_str_to_bytebuffer;
    // long names push the page over PACKET_SIZE_SOFT_LIMIT before 32 entries
    let names: Vec<String> = (1..=32).map(|i| format!("{i:02}{}", "x".repeat(40))).collect();
    let (more, body) = serialize_page(32, names.iter().enumerate(), |(index, name), bytes| {
      Some(get_transmit_channels_friendly_names::ChannelDescriptor {
        channel_id_1: (index + 1).try_into().unwrap(),
        channel_id_2: (index + 1).try_into().unwrap(),
        friendly_name_offset: write_0term_str_to_bytebuffer(bytes, name),
      })
    });
    assert!(more);
    let count = body[1] as usize;
    assert!(count > 0 && count < 32);
    let table_end = HEADER_LENGTH + 2 + count * 6;
    let first_name = u16::from_be_bytes([body[6], body[7]]) as usize;
    assert_eq!(first_name, table_end);
    assert!(body.len() + HEADER_LENGTH < PACKET_SIZE_SOFT_LIMIT + 64);
  }

  #[test]
  fn soft_limit_cut_page_has_no_orphaned_bytes() {
    use crate::byte_utils::write_0term_str_to_bytebuffer;
    // The entry that crosses PACKET_SIZE_SOFT_LIMIT goes on the next page;
    // the name it already wrote must not stay behind in this one.
    let names: Vec<String> = (1..=32).map(|i| format!("{i:02}{}", "x".repeat(40))).collect();
    let (more, body) = serialize_page(32, names.iter().enumerate(), |(index, name), bytes| {
      Some(get_transmit_channels_friendly_names::ChannelDescriptor {
        channel_id_1: (index + 1).try_into().unwrap(),
        channel_id_2: (index + 1).try_into().unwrap(),
        friendly_name_offset: write_0term_str_to_bytebuffer(bytes, name),
      })
    });
    assert!(more);
    let count = body[1] as usize;
    // every byte after the table belongs to an emitted entry's name: the
    // packet ends with the last emitted name's terminator
    let last = 2 + (count - 1) * 6;
    let last_name = u16::from_be_bytes([body[last + 4], body[last + 5]]) as usize - HEADER_LENGTH;
    let end = last_name + names[count - 1].len() + 1;
    assert_eq!(body.len(), end, "bytes after the last emitted entry's data");
    assert_eq!(&body[last_name..end - 1], names[count - 1].as_bytes());
    assert!(body.len() + HEADER_LENGTH < PACKET_SIZE_SOFT_LIMIT + 64);
  }

  #[test]
  fn receive_pages_hold_at_most_16_entries() {
    // the unit's 32-channel receive list must go out as two full pages of 16,
    // not one page of 24 followed by zeroed slots
    assert_eq!(RX_CHANNELS_PAGE_SIZE, 16);
    let page: u8 = 32usize.min(RX_CHANNELS_PAGE_SIZE).try_into().unwrap();
    let (more, body) = serialize_page(page, 0..32u16, |i, _| {
      Some(get_receive_channels::ChannelDescriptor { channel_id: i + 1, ..Default::default() })
    });
    assert!(more);
    assert_eq!((body[0], body[1]), (16, 16));
  }
}

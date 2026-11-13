/*
start_code = 0x1102 = dbcp1

error responses:
opcode2 = 0103 = stream expired (i.e. no keepalives)
opcode2 = 0315 = too many tx flows
opcode2 = 0301 = sample rate mismatch
*/

use log::{error, warn};
use std::{
  error::Error,
  io::ErrorKind,
  net::{IpAddr, SocketAddr},
  sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
  },
  time::Duration,
};

use crate::device_info::DeviceInfo;
use crate::net_utils::MTU;
use bytebuffer::ByteBuffer;
use thiserror::Error;
use tokio::{
  net::UdpSocket,
  time::{timeout_at, Instant},
};

use super::req_resp::{make_packet, req_resp_packet, HEADER_LENGTH};

pub const PORT: u16 = 4455;

#[derive(Error, Debug)]
pub enum FlowControlError {
  #[error("flow not found")]
  FlowNotFound = 0x0103,
  #[error("too many TX flows")]
  TooManyTXFlows = 0x0315,
  #[error("sample rate mismatch")]
  SampleRateMismatch = 0x0301,
}

pub struct FlowsControlClient {
  seqnum: AtomicU16,
  self_info: Arc<DeviceInfo>,
}

pub type FlowHandle = [u8; 6];

/// Why a flow-control request from the network was rejected before acting on it.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum RequestParseError {
  #[error("packet too short ({len} bytes, need {need})")]
  TooShort { len: usize, need: usize },
  #[error("{field} offset {offset} is outside the packet")]
  BadOffset { field: &'static str, offset: usize },
  #[error("{field} is not valid UTF-8")]
  BadString { field: &'static str },
  #[error("{field} is {len} bytes, longer than {MAX_REQUEST_NAME_LEN}")]
  StringTooLong { field: &'static str, len: usize },
}

/// Longest receiver host or flow name a flow request may carry. Device and
/// flow names on the network are at most 31 characters; this leaves room
/// for multi-byte UTF-8. The names are stored with the flow and echoed back
/// in ARC transmit-flow pages, so an unbounded one (up to a whole datagram)
/// made a page larger than the send buffer.
pub const MAX_REQUEST_NAME_LEN: usize = 63;

/// A request flow (opcode 0x0100) as received by a transmitter.
#[derive(Debug, PartialEq, Eq)]
pub struct FlowRequest<'a> {
  pub rx_hostname: &'a str,
  pub rx_flow_name: &'a str,
  pub sample_rate: u32,
  pub bits_per_sample: u32,
  /// Local (transmitter) channel index per channel in the flow; None for an empty slot.
  pub channel_indices: Vec<Option<usize>>,
  pub fpp: u16,
  pub rx_ip: std::net::Ipv4Addr,
  pub rx_port: u16,
}

fn channel_ids_to_indices(bytes: &[u8]) -> Vec<Option<usize>> {
  bytes
    .chunks_exact(2)
    .map(|c| match u16::from_be_bytes([c[0], c[1]]) {
      0 => None,
      id => Some(id as usize - 1),
    })
    .collect()
}

fn string_at<'a>(
  packet: &'a [u8],
  offset: usize,
  field: &'static str,
) -> Result<&'a str, RequestParseError> {
  if offset >= packet.len() {
    return Err(RequestParseError::BadOffset { field, offset });
  }
  let s = crate::byte_utils::read_0term_str_from_buffer(packet, offset)
    .map_err(|_| RequestParseError::BadString { field })?;
  if s.len() > MAX_REQUEST_NAME_LEN {
    return Err(RequestParseError::StringTooLong { field, len: s.len() });
  }
  Ok(s)
}

/// Parses a request flow packet (whole packet, header included: string and
/// descriptor offsets in it are relative to the packet start). Every length,
/// count and offset comes from the network and is checked before use.
///
/// Content layout after the 10-byte header:
/// `u16 hostname_offset, u32 sample_rate, u32 bits, u16 1, u16 n, u16 descr_offset,
///  n x u16 channel id, 6 bytes, u16 fpp, u16 flow_name_offset, ...`
pub fn parse_flow_request(packet: &[u8]) -> Result<FlowRequest<'_>, RequestParseError> {
  let c = packet.get(HEADER_LENGTH..).unwrap_or(&[]);
  let too_short = |need| RequestParseError::TooShort { len: c.len(), need };
  if c.len() < 16 {
    return Err(too_short(16));
  }
  let be16 = |i: usize| u16::from_be_bytes([c[i], c[i + 1]]);
  let be32 = |i: usize| u32::from_be_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]);
  let hostname_offset = be16(0) as usize;
  let sample_rate = be32(2);
  let bits_per_sample = be32(6);
  let one = be16(10);
  if one != 1 {
    warn!("flow request: expecting 1, received {one:#x}");
  }
  let num_channels = be16(12) as usize;
  let remote_descr_offset = be16(14) as usize;
  let channels_end = 16 + num_channels * 2;
  let fpp_pos = channels_end + 6;
  if c.len() < fpp_pos + 4 {
    return Err(too_short(fpp_pos + 4));
  }
  let channel_indices = channel_ids_to_indices(&c[16..channels_end]);
  let fpp = be16(fpp_pos);
  let rx_flow_name_offset = be16(fpp_pos + 2) as usize;

  let rx_hostname = string_at(packet, hostname_offset, "hostname")?;
  let rx_flow_name = string_at(packet, rx_flow_name_offset, "rx flow name")?;

  let descr = packet
    .get(remote_descr_offset..remote_descr_offset + 8)
    .ok_or(RequestParseError::BadOffset { field: "receiver address", offset: remote_descr_offset })?;
  if descr[0..2] != [0x08, 0x02] {
    warn!("flow request: expected 0x0802, got 0x{:02x}{:02x}", descr[0], descr[1]);
  }
  let rx_port = u16::from_be_bytes([descr[2], descr[3]]);
  let rx_ip = std::net::Ipv4Addr::new(descr[4], descr[5], descr[6], descr[7]);

  Ok(FlowRequest {
    rx_hostname,
    rx_flow_name,
    sample_rate,
    bits_per_sample,
    channel_indices,
    fpp,
    rx_ip,
    rx_port,
  })
}

/// Parses an update flow request (opcode 0x0102) content:
/// `6-byte handle, u16 n, n x u16 channel id`.
pub fn parse_flow_update(content: &[u8]) -> Result<(FlowHandle, Vec<Option<usize>>), RequestParseError> {
  if content.len() < 8 {
    return Err(RequestParseError::TooShort { len: content.len(), need: 8 });
  }
  let handle: FlowHandle = content[0..6].try_into().unwrap();
  let num_channels = u16::from_be_bytes([content[6], content[7]]) as usize;
  let need = 8 + num_channels * 2;
  if content.len() < need {
    return Err(RequestParseError::TooShort { len: content.len(), need });
  }
  Ok((handle, channel_ids_to_indices(&content[8..need])))
}

impl FlowsControlClient {
  pub fn new(self_info: Arc<DeviceInfo>) -> Self {
    Self { seqnum: AtomicU16::new(1), self_info }
  }
  async fn connect(&self, rem_addr: &SocketAddr) -> tokio::io::Result<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(self.self_info.ip_address), 0)).await?;
    socket.connect(rem_addr).await?;
    return Ok(socket);
  }
  fn write_channels(buffer: &mut ByteBuffer, channels: &[Option<u16>]) {
    for ch in channels {
      buffer.write_u16(ch.unwrap_or(0));
    }
  }
  async fn send_and_wait_for_reply<'a>(
    &self,
    recvbuf: &'a mut [u8],
    socket: &UdpSocket,
    _start_code: u16, // looks like version number, so we don't use it, forcing our version instead
    opcode1: u16,
    content: &[u8],
  ) -> Result<req_resp_packet::View<&'a [u8]>, Box<dyn Error>> {
    let send_seqnum = self.seqnum.fetch_add(1, Ordering::AcqRel);
    let pkt_to_send = make_packet(recvbuf, /*start_code*/ 0x1102, send_seqnum, opcode1, 0, content);
    socket.send(pkt_to_send).await?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
      let recv_size = match timeout_at(deadline, socket.recv(recvbuf)).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
          return Err(Box::new(e));
        }
        Err(_) => {
          return Err(Box::new(std::io::Error::from(ErrorKind::TimedOut)));
        }
      };
      if recv_size < HEADER_LENGTH {
        return Err(Box::new(std::io::Error::from(ErrorKind::UnexpectedEof)));
      }
      {
        let resp = req_resp_packet::View::new(&recvbuf[..recv_size]);
        if resp.opcode1().read() == opcode1 && resp.seqnum().read() == send_seqnum {
          if resp.opcode2().read() == 1 {
            return Ok(req_resp_packet::View::new(&recvbuf[..recv_size]));
            // creating another View is necessary because https://users.rust-lang.org/t/solved-borrow-doesnt-drop-returning-this-value-requires-that/24182
          } else {
            error!("server returned error: {}", hex::encode(&resp.into_storage()));
            return Err(Box::new(std::io::Error::from(ErrorKind::InvalidData)));
          }
        } else {
          warn!("received spurious packet: {}", hex::encode(&resp.into_storage()));
        }
      }
    }
  }
  pub async fn request_flow(
    &self,
    rem_addr: &SocketAddr,
    dbcp1: u16,
    sample_rate: u32,
    bits_per_sample: u32,
    fpp: u16,
    channels: &[Option<u16>],
    rx_port: u16,
    rx_flow_name: &str,
  ) -> Result<FlowHandle, Box<dyn Error>> {
    let socket = self.connect(rem_addr).await?;
    let body = self.flow_request_body(sample_rate, bits_per_sample, fpp, channels, rx_port, rx_flow_name);

    let mut recvbuf = [0; MTU];
    let resp = self.send_and_wait_for_reply(&mut recvbuf, &socket, dbcp1, 0x0100, &body).await?;
    if resp.content().len() < 6 {
      return Err(Box::new(std::io::Error::from(ErrorKind::UnexpectedEof)));
    }
    let mut handle = [0u8; 6];
    handle.copy_from_slice(&resp.content()[0..6]);
    return Ok(handle);
  }

  /// Content of a request flow packet (opcode 0x0100); `parse_flow_request` is its inverse.
  fn flow_request_body(
    &self,
    sample_rate: u32,
    bits_per_sample: u32,
    fpp: u16,
    channels: &[Option<u16>],
    rx_port: u16,
    rx_flow_name: &str,
  ) -> Vec<u8> {
    let mut body = ByteBuffer::new();
    let mut strings = ByteBuffer::new();
    let strings_offset = 0x26 + channels.len() * 2 + HEADER_LENGTH;
    body.write_u16(strings_offset as u16);
    strings.write_bytes(self.self_info.friendly_hostname.as_bytes());
    strings.write_u8(0);
    let rx_flow_name_offset = (strings.get_wpos() + strings_offset) as u16;
    strings.write_bytes(rx_flow_name.as_bytes());
    strings.write_u8(0);
    body.write_u32(sample_rate);
    body.write_u32(bits_per_sample);
    body.write_u16(1);
    body.write_u16(channels.len() as u16);
    while (strings.get_wpos() + strings_offset) % 8 != 0 {
      strings.write_u8(0);
    }
    body.write_u16((strings.get_wpos() + strings_offset) as u16);
    strings.write_u16(0x0802);
    strings.write_u16(rx_port);
    strings.write_bytes(&self.self_info.ip_address.octets());
    Self::write_channels(&mut body, channels);
    body.write_u16((0x1c + 2 * channels.len()) as u16);
    body.write_u16(0x0a00);
    body.write_u16(0x0002);
    body.write_u16(fpp);
    body.write_u16(rx_flow_name_offset);
    /* for _ in 0..6 {
      body.write_u16(self.self_info.process_id); // XXX testing
    } */
    body.write_bytes(&[0; 12]); // XXX testing
    assert_eq!(body.get_wpos(), strings_offset - HEADER_LENGTH);
    body.write_bytes(strings.as_bytes());
    body.into_vec()
  }

  pub async fn update_flow(
    &self,
    rem_addr: &SocketAddr,
    dbcp1: u16,
    handle: FlowHandle,
    channels: &[Option<u16>],
  ) -> Result<(), Box<dyn Error>> {
    let socket = self.connect(rem_addr).await?;
    let mut body = ByteBuffer::new();
    body.write_bytes(&handle);
    body.write_u16(channels.len() as u16);
    Self::write_channels(&mut body, channels);
    let mut recvbuf = [0; MTU];
    return self
      .send_and_wait_for_reply(&mut recvbuf, &socket, dbcp1, 0x0102, body.as_bytes())
      .await
      .map(|_| ());
  }

  pub async fn stop_flow(
    &self,
    rem_addr: &SocketAddr,
    dbcp1: u16,
    handle: FlowHandle,
  ) -> Result<(), Box<dyn Error>> {
    let socket = self.connect(rem_addr).await?;
    let mut body = ByteBuffer::new();
    body.write_bytes(&handle);
    let mut recvbuf = [0; MTU];
    return self
      .send_and_wait_for_reply(&mut recvbuf, &socket, dbcp1, 0x0101, body.as_bytes())
      .await
      .map(|_| ());
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use netdev::mac::MacAddr;
  use std::net::Ipv4Addr;

  fn minimal_device_info() -> DeviceInfo {
    DeviceInfo {
      ip_address: Ipv4Addr::new(127, 0, 0, 1),
      netmask: Ipv4Addr::new(0, 0, 0, 0),
      gateway: Ipv4Addr::new(0, 0, 0, 0),
      mac_address: MacAddr::zero(),
      link_speed: 0,
      board_name: String::new(),
      manufacturer: String::new(),
      model_name: String::new(),
      model_number: String::new(),
      factory_device_id: [0; 8],
      process_id: 0,
      vendor_string: String::new(),
      friendly_hostname: String::new(),
      factory_hostname: String::new(),
      rx_channels: Vec::new(),
      tx_channels: Vec::new(),
      bits_per_sample: 0,
      pcm_type: 0,
      latency_ns: 0,
      sample_rate: 0,
      arc_port: 0,
      cmc_port: 0,
      flows_control_port: 0,
      info_request_port: 0,
      product_version: None,
      name_request_path: None,
      latency_request_path: None,
      sample_rate_request_path: None,
      announced_sample_rate: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
      announced_latency_ns: Default::default(),
      state_dir: None,
    }
  }

  #[test]
  fn write_channels_empty_writes_nothing() {
    let _client = FlowsControlClient::new(Arc::new(minimal_device_info()));
    let mut buffer = ByteBuffer::new();
    FlowsControlClient::write_channels(&mut buffer, &[]);
    assert_eq!(buffer.as_bytes(), &[]);
  }

  #[test]
  fn write_channels_some_none() {
    let _client = FlowsControlClient::new(Arc::new(minimal_device_info()));
    let mut buffer = ByteBuffer::new();
    FlowsControlClient::write_channels(&mut buffer, &[Some(1), Some(2), None, Some(4)]);
    assert_eq!(buffer.as_bytes(), &[0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x04]);
  }

  fn request_packet(channels: &[Option<u16>]) -> Vec<u8> {
    let mut info = minimal_device_info();
    info.friendly_hostname = "rx-host".to_owned();
    info.ip_address = Ipv4Addr::new(192, 168, 1, 7);
    let client = FlowsControlClient::new(Arc::new(info));
    let body = client.flow_request_body(48000, 24, 32, channels, 5004, "1_42");
    let mut buf = [0u8; MTU];
    make_packet(&mut buf, 0x1102, 1, 0x0100, 0, &body).to_vec()
  }

  #[test]
  fn parse_flow_request_roundtrip() {
    let packet = request_packet(&[Some(1), None, Some(8)]);
    let req = parse_flow_request(&packet).unwrap();
    assert_eq!(req.rx_hostname, "rx-host");
    assert_eq!(req.rx_flow_name, "1_42");
    assert_eq!(req.sample_rate, 48000);
    assert_eq!(req.bits_per_sample, 24);
    assert_eq!(req.channel_indices, vec![Some(0), None, Some(7)]);
    assert_eq!(req.fpp, 32);
    assert_eq!(req.rx_ip, Ipv4Addr::new(192, 168, 1, 7));
    assert_eq!(req.rx_port, 5004);
  }

  #[test]
  fn parse_flow_request_rejects_every_truncation() {
    let packet = request_packet(&[Some(1), Some(2), Some(3), Some(4)]);
    for len in 0..packet.len() {
      // the strings and the address descriptor sit at the end, so every prefix is incomplete
      assert!(parse_flow_request(&packet[..len]).is_err(), "prefix of {len} bytes accepted");
    }
  }

  #[test]
  fn parse_flow_request_channel_count_past_end() {
    let mut packet = request_packet(&[Some(1)]);
    // claim 0xffff channels: the old handler indexed c[16 + i*2] for all of them
    packet[HEADER_LENGTH + 12] = 0xff;
    packet[HEADER_LENGTH + 13] = 0xff;
    assert!(matches!(parse_flow_request(&packet), Err(RequestParseError::TooShort { .. })));
  }

  #[test]
  fn parse_flow_request_offsets_outside_packet() {
    let base = request_packet(&[Some(1)]);
    // hostname offset, receiver descriptor offset, rx flow name offset (after 1 channel)
    for field_pos in [0usize, 14, 16 + 2 + 6 + 2] {
      for offset in [base.len() as u16, base.len() as u16 - 3, 0xffff] {
        let mut packet = base.clone();
        packet[HEADER_LENGTH + field_pos..HEADER_LENGTH + field_pos + 2]
          .copy_from_slice(&offset.to_be_bytes());
        assert!(parse_flow_request(&packet).is_err(), "field at {field_pos}, offset {offset}");
      }
    }
  }

  #[test]
  fn parse_flow_request_never_panics_on_mutations() {
    use rand::{rngs::SmallRng, Rng, SeedableRng};
    let base = request_packet(&[Some(1), Some(2)]);
    let mut rng = SmallRng::seed_from_u64(49);
    for _ in 0..20_000 {
      let mut packet = base.clone();
      for _ in 0..rng.gen_range(1..6) {
        let i = rng.gen_range(0..packet.len());
        packet[i] = rng.gen();
      }
      packet.truncate(rng.gen_range(0..=packet.len()));
      let _ = parse_flow_request(&packet);
    }
  }

  #[test]
  fn parse_flow_request_rejects_overlong_names() {
    let mut info = minimal_device_info();
    info.ip_address = Ipv4Addr::new(192, 168, 1, 7);
    for (host, flow) in [("h".repeat(64), "1_42".to_owned()), ("rx-host".to_owned(), "f".repeat(64))] {
      info.friendly_hostname = host;
      let client = FlowsControlClient::new(Arc::new(info.clone()));
      let body = client.flow_request_body(48000, 24, 32, &[Some(1)], 5004, &flow);
      let mut buf = [0u8; MTU];
      let packet = make_packet(&mut buf, 0x1102, 1, 0x0100, 0, &body).to_vec();
      assert!(matches!(parse_flow_request(&packet), Err(RequestParseError::StringTooLong { len: 64, .. })));
    }
    info.friendly_hostname = "h".repeat(MAX_REQUEST_NAME_LEN);
    let client = FlowsControlClient::new(Arc::new(info));
    let body = client.flow_request_body(48000, 24, 32, &[Some(1)], 5004, "1_42");
    let mut buf = [0u8; MTU];
    let packet = make_packet(&mut buf, 0x1102, 1, 0x0100, 0, &body).to_vec();
    assert_eq!(parse_flow_request(&packet).unwrap().rx_hostname.len(), MAX_REQUEST_NAME_LEN);
  }

  #[test]
  fn parse_flow_update_valid_and_short() {
    let content = [1, 2, 3, 4, 5, 6, 0, 2, 0, 3, 0, 0];
    assert_eq!(parse_flow_update(&content), Ok(([1, 2, 3, 4, 5, 6], vec![Some(2), None])));
    for len in 0..content.len() {
      assert!(parse_flow_update(&content[..len]).is_err(), "prefix of {len} bytes accepted");
    }
    let mut huge = content.to_vec();
    huge[6] = 0xff;
    assert!(parse_flow_update(&huge).is_err());
  }

  #[test]
  fn write_channels_many() {
    let _client = FlowsControlClient::new(Arc::new(minimal_device_info()));
    let mut buffer = ByteBuffer::new();
    let channels: Vec<Option<u16>> = (0..100).map(|i| Some(i)).collect();
    FlowsControlClient::write_channels(&mut buffer, &channels);
    assert_eq!(buffer.get_wpos(), 200);
  }
}

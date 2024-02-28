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
use tokio::{
  net::UdpSocket,
  time::{timeout_at, Instant},
};
use thiserror::Error;

use super::req_resp::{make_packet, req_resp_packet, HEADER_LENGTH};

pub const PORT: u16 = 4455;


#[derive(Error, Debug)]
pub enum FlowControlError {
  #[error("flow not found")]
  FlowNotFound = 0x0103,
  #[error("too many TX flows")]
  TooManyTXFlows = 0x0315,
  #[error("sample rate mismatch")]
  SampleRateMismatch = 0x0301
}

pub struct FlowsControlClient {
  seqnum: AtomicU16,
  self_info: Arc<DeviceInfo>,
}

pub type FlowHandle = [u8; 6];

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
    let pkt_to_send =
      make_packet(recvbuf, /*start_code*/ 0x1102, send_seqnum, opcode1, 0, content);
    socket.send(pkt_to_send).await?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
      let recv_size = match timeout_at(deadline, socket.recv(recvbuf)).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
          return Err(Box::new(e));
        }
        Err(e) => {
          return Err(Box::new(e));
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
    body.write_bytes(&[0; 12]);
    assert_eq!(body.get_wpos(), strings_offset - HEADER_LENGTH);
    body.write_bytes(strings.as_bytes());

    let mut recvbuf = [0; MTU];
    let resp =
      self.send_and_wait_for_reply(&mut recvbuf, &socket, dbcp1, 0x0100, body.as_bytes()).await?;
    if resp.content().len() < 6 {
      return Err(Box::new(std::io::Error::from(ErrorKind::UnexpectedEof)));
    }
    let mut handle = [0u8; 6];
    handle.copy_from_slice(&resp.content()[0..6]);
    return Ok(handle);
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

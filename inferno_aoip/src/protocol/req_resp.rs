use crate::common::*;
use binary_layout::prelude::*;
use binary_serde::recursive_array::RecursiveArray as _;
use binary_serde::BinarySerde;
use std::net::SocketAddr;
use std::{borrow::BorrowMut, io::Cursor};

use crate::net_utils::{ReceiveBuffer, UdpSocketWrapper, MTU};

pub const HEADER_LENGTH: usize = 10;
const SEND_BUFFER_SIZE: usize = MTU;
pub const CODE_OK: u16 = 1;

define_layout!(req_resp_packet, BigEndian, {
  start_code: u16,
  total_length: u16,
  seqnum: u16,
  opcode1: u16,
  opcode2: u16,
  content: [u8]
});

#[derive(Clone)]
struct RemoteInfo {
  addr: SocketAddr,
  start_code: u16,
  seqnum: u16,
  opcode1: u16,
}

pub struct Connection {
  server: UdpSocketWrapper,
  send_buff: [u8; SEND_BUFFER_SIZE],
  remote: Option<RemoteInfo>,
}

pub fn make_packet<'a>(
  buf: &'a mut [u8],
  start_code: u16,
  seqnum: u16,
  opcode1: u16,
  opcode2: u16,
  content: &[u8],
) -> &'a [u8] {
  let total_len = content.len() + HEADER_LENGTH;
  assert!(total_len < (1 << 16)); // TODO MAY PANIC
  let buffer = &mut buf[..total_len]; // TODO MAY PANIC check length before slicing
  let mut view = req_resp_packet::View::new(buffer);
  view.start_code_mut().write(start_code);
  view.total_length_mut().write(total_len as u16);
  view.seqnum_mut().write(seqnum);
  view.opcode1_mut().write(opcode1);
  view.opcode2_mut().write(opcode2);
  view.content_mut().copy_from_slice(&content);
  return view.into_storage();
}

impl Connection {
  pub fn new(server: UdpSocketWrapper) -> Connection {
    Connection { server, send_buff: [0; SEND_BUFFER_SIZE], remote: None }
  }

  pub fn should_work(&self) -> bool {
    return self.server.should_work();
  }

  pub async fn recv<'a>(
    &mut self,
    recv_buff: &'a mut ReceiveBuffer,
  ) -> Option<req_resp_packet::View<&'a [u8]>> {
    let (src, request_buf) = match self.server.borrow_mut().recv(recv_buff).await {
      Some(v) => v,
      None => {
        return None;
      }
    };
    if request_buf.len() < HEADER_LENGTH {
      error!("received too short packet: {}", hex::encode(request_buf));
      return None;
    }
    let view = req_resp_packet::View::new(request_buf);
    self.remote = Some(RemoteInfo {
      addr: src,
      start_code: view.start_code().read(),
      seqnum: view.seqnum().read(),
      opcode1: view.opcode1().read(),
    });
    return Some(view);
  }

  pub async fn send(
    &mut self,
    dst: SocketAddr,
    start_code: u16,
    seqnum: u16,
    opcode1: u16,
    opcode2: u16,
    content: &[u8],
  ) {
    // make_packet asserts the packet fits the buffer; a response built from
    // network-supplied data must never take the server (and, in the ALSA
    // plugin, the host process) down with it.
    if content.len() + HEADER_LENGTH > self.send_buff.len() {
      error!(
        "dropping {} byte response to opcode {opcode1:#06x}: larger than the {} byte send buffer",
        content.len() + HEADER_LENGTH,
        self.send_buff.len()
      );
      return;
    }
    let pkt = make_packet(&mut self.send_buff, start_code, seqnum, opcode1, opcode2, content);
    self.server.send(&dst, pkt).await;
  }
  pub async fn respond(&mut self, payload: &[u8]) {
    self.respond_with_code(1, payload).await;
  }
  pub async fn respond_with_code(&mut self, opcode2: u16, content: &[u8]) {
    let rem = self.remote.as_ref().unwrap();
    self.send(rem.addr, rem.start_code, rem.seqnum, rem.opcode1, opcode2, content).await;
  }
  pub async fn respond_with_struct(&mut self, code: u16, payload: impl BinarySerde) {
    self
      .respond_with_code(
        code,
        payload.binary_serialize_to_array(binary_serde::Endianness::Big).as_slice(),
      )
      .await;
  }
}

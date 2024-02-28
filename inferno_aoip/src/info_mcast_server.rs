use crate::common::*;
use crate::byte_utils::*;
use crate::net_utils::UdpSocketWrapper;
use crate::protocol::mcast::{make_packet, INFO_REQUEST_PORT};
use crate::{byte_utils::write_str_to_buffer, device_info::DeviceInfo};
use std::{
  net::{IpAddr, Ipv4Addr, SocketAddr},
  sync::Arc,
  time::Duration,
};
use tokio::time::interval;
use tokio::{
  select,
  sync::{broadcast::Receiver as BroadcastReceiver, mpsc},
  time::MissedTickBehavior,
};

const BIND_PORT: u16 = INFO_REQUEST_PORT;
const SEND_BUFFER_SIZE: usize = 1500;
const DST_PORT_HEARTBEAT: u16 = 8708;
const DST_PORT_DEVICE_INFO: u16 = 8702;

pub struct MulticastMessage {
  pub start_code: u16,
  pub opcode: [u8; 8],
  pub content: Vec<u8>,
}

struct Multicaster<'s> {
  self_info: &'s DeviceInfo,
  pub server: UdpSocketWrapper,
  seqnum: u16,
  vendor: [u8; 8],
  library_version_bytes: [u8; 4],
  device_info_destination: SocketAddr,
  heartbeat_destination: SocketAddr,
  send_buffer: [u8; SEND_BUFFER_SIZE],
}

impl<'s> Multicaster<'s> {
  pub fn new(self_info: &'s DeviceInfo, server: UdpSocketWrapper) -> Multicaster {
    let patch_version = env!("CARGO_PKG_VERSION_PATCH").parse::<u16>().unwrap();
    let mut r = Multicaster {
      self_info,
      server,
      seqnum: 1,
      vendor: [32; 8],
      library_version_bytes: [
        env!("CARGO_PKG_VERSION_MAJOR").parse::<u8>().unwrap(),
        env!("CARGO_PKG_VERSION_MINOR").parse::<u8>().unwrap(),
        H(patch_version),
        L(patch_version)
      ],
      device_info_destination: SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(224, 0, 0, 231)),
        DST_PORT_DEVICE_INFO,
      ),
      heartbeat_destination: SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(224, 0, 0, 233)),
        DST_PORT_HEARTBEAT,
      ),
      send_buffer: [0; SEND_BUFFER_SIZE],
    };
    write_str_to_buffer(&mut r.vendor, 0, 8, &self_info.vendor_string);
    return r;
  }

  pub fn should_work(&self) -> bool {
    return self.server.should_work();
  }

  async fn send(&mut self, dst: SocketAddr, start_code: u16, opcode: [u8; 8], content: &[u8]) {
    let pkt = make_packet(
      &mut self.send_buffer,
      start_code,
      self.seqnum,
      self.self_info.factory_device_id,
      self.vendor,
      opcode,
      content,
    );
    self.seqnum = self.seqnum.wrapping_add(1);
    self.server.send(&dst, pkt).await;
  }

  async fn send_board_info(&mut self) {
    let mut content = [0u8; 200];
    // Firmware version:
    content[0..4].copy_from_slice(&self.library_version_bytes);
    content[0x23] = 0;
    // Hardware version:
    content[4..8].copy_from_slice(&[1, 3, 0, 3]);
    content[0x27] = 7;
    // Boot version:
    content[0x28..0x2c].copy_from_slice(&[1, 0, 0, 0]);

    // flags of supported features:
    // 0x14: AES67, Device Lock
    //       0x01 - ??? (was 1)
    //       0x04 - supports AES67
    //       0x08 - is lockable
    // 0x15: ??? (was 0x50)
    // 0x16:
    //       0x10 - has Manufacturer name
    //       0x40 - Network is configurable (supports static addressing)
    // 0x17: Identify device, Sample rate & encoding configuration (was 0xdb)
    content[0x14] = 0;
    content[0x15] = 0;
    content[0x16] = 0x10;
    content[0x17] = 0;

    content[0xbb] = 0x1f; // if 0, device is flooded with info multicast requests around 1 per second
    write_str_to_buffer(&mut content, 12, 8, &self.self_info.board_name);
    write_str_to_buffer(&mut content, 0x38, 16, &self.self_info.board_name);

    self
      .send(self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x60, 0, 0, 0, 0], &content)
      .await;
  }

  async fn send_product_info(&mut self) {
    let mut content = [0; 336];
    write_str_to_buffer(&mut content, 0, 8, &self.self_info.manufacturer);
    write_str_to_buffer(&mut content, 8, 8, &self.self_info.board_name);
    write_str_to_buffer(&mut content, 0x2c, 16, &self.self_info.manufacturer);
    write_str_to_buffer(&mut content, 0xac, 16, &self.self_info.model_name);
    // version number:
    content[0x12c..0x130].copy_from_slice(&[0, 0, 0, 0]);

    self
      .send(self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0xc0, 0, 0, 0, 0], &content)
      .await;
  }

  async fn send_heartbeat(&mut self) {
    let ctr = self.seqnum;
    let content = [
      /*len, 2B:*/0x00, 0x10,
      /*type, 2B:*/0x80, 0x01,
      0x00, 0x04, 0x00, 0x04,
      H(ctr), L(ctr), 0x00, 0x00, /*freq offset, in 1e-9 (ppm*1000), 4B: */ 0, 0, 0, 0,
      
      /*0x00, 0x24, 0x80, 0x00, 0x00, 0x04, 0x00, 0x04,
      H(ctr), L(ctr), 0x00, 0x00, 0x00, 0x10, 0x00, 0x00,
      0x00, 0x01, 0x00, 0x10, 0, 0, 0, 0,
      0, 0, 0, 0, 0x00, 0x00, 0x00, 0x00,
      0x00, 0x00, 0x00, 0x00,
      
      0x00, 0x1c, 0x80, 0x02, 0x00, 0x04, 0x00, 0x10,
      H(ctr), L(ctr), 0x00, 0x00, 0, 0, 0, 0,
      0, 0, 0, 0, 0, 0, 0, 0,
      0, 0, 0, 0,*/
      
      /*0x00, 0x20, 0x80, 0x03, 0x00, 0x04, 0x00, 0x14,
      H(ctr), L(ctr), 0x00, 0x00, 0x00, 0x02, 0x00, 0x00,
      0x00, /*0x18*/0, 0x00, 0x00, 0x00, 0x00, 0xbb, 0x80,
      0x00, 0x00, 0x00, /*0x14*/0, 0x00, 0x00, 0x00, 0x00,*/
      
      /*0x00, 0x1c, 0x80, 0x04, 0x00, 0x04, 0x00, 0x10,
      H(ctr), L(ctr), 0x00, 0x00, 0x00, 0x02, 0x00, 0x00,
      0, 0, 0, 0, 0, 0, 0, 0,
      0, 0, 0, 0*/
    ];
    self
      .send(
        self.heartbeat_destination,
        0xfffe,
        [0, 8, 0, 1, 0x10, 0, 0, 0],
        &content,
      )
      .await;
  }
}

pub async fn run_server(
  self_info: Arc<DeviceInfo>,
  mut rx: mpsc::Receiver<MulticastMessage>,
  shutdown: BroadcastReceiver<()>,
) {
  let server = UdpSocketWrapper::new(Some(self_info.ip_address), BIND_PORT, shutdown);
  let mut mcaster = Multicaster::new(self_info.as_ref(), server);
  mcaster.send_board_info().await;
  mcaster.send_product_info().await;
  let mut heartbeat_interval = interval(Duration::from_secs(1));
  heartbeat_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
  while mcaster.should_work() {
    select! {
      r = mcaster.server.recv() => {
        let (_src, request_buf) = match r {
          Some(v) => v,
          None => continue
        };
        if request_buf.len() < 32 {
          error!("too short packet received: {}", hex::encode(request_buf));
          continue;
        }
        let opcode = &request_buf[24..32];
        match opcode {
          [0x07, _, 0, 0x61, 0, 0, 0, 0] => {
            mcaster.send_board_info().await;
          },
          [0x07, _, 0, 0xc1, 0, 0, 0, 0] => {
            mcaster.send_product_info().await;
          },
          _ => {
            warn!("unknown request to multicast port: {}", hex::encode(request_buf));
          }
        };
      },
      m = rx.recv() => {
        // TODO we could also make seqnum atomic and simply share socket with anyone that wants it
        if let Some(msg) = m {
          mcaster.send(mcaster.device_info_destination, msg.start_code, msg.opcode, &msg.content).await;
        } else {
          break;
        }
      },
      _ = heartbeat_interval.tick() => {
        mcaster.send_heartbeat().await;
      }
      // TODO receive shutdown properly, currently Ctrl-C doesn't work if there is error binding to socket
    };
  }
}

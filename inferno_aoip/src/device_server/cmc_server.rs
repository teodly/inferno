use crate::common::*;
use crate::protocol::req_resp::CODE_OK;
use std::sync::Arc;

use crate::device_info::DeviceInfo;
use crate::net_utils::UdpSocketWrapper;
use crate::protocol::proto_cmc::*;
use crate::protocol::req_resp;
use bytebuffer::ByteBuffer;
use tokio::sync::broadcast::Receiver as BroadcastReceiver;


/// Parse the trailing self-record of a legacy `0x1200` plane request into
/// primary/secondary (ip, port) socket addresses. Each record is 8 bytes:
/// u32 IPv4, u16 port, u16 reserved (0).
pub fn parse_legacy_self_record(content: &[u8]) -> Vec<std::net::SocketAddr> {
  let mut out = Vec::new();
  if content.len() < 16 {
    return out;
  }
  let tail = &content[content.len() - 16..];
  for chunk in tail.chunks(8) {
    let ip = std::net::Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
    let port = u16::from_be_bytes([chunk[4], chunk[5]]);
    if port != 0 {
      out.push(std::net::SocketAddr::new(std::net::IpAddr::V4(ip), port));
    }
  }
  out
}

pub async fn run_server(self_info: Arc<DeviceInfo>, shutdown: BroadcastReceiver<()>) {
  let server = UdpSocketWrapper::new(Some(self_info.ip_address), self_info.cmc_port, shutdown).await;
  let mut conn = req_resp::Connection::new(server);
  let mut recv_buff = crate::net_utils::ReceiveBuffer::new();
  while conn.should_work() {
    let request = match conn.recv(&mut recv_buff).await {
      Some(v) => v,
      None => continue,
    };

    // Legacy discovery plane (0x1200@8800): a peer (e.g. a Yamaha DSP)
    // probes by device name. Treat it as its own plane: log as a known wire
    // variant, parse the embedded self-record (primary/secondary IPv4 + port)
    // and reply to those addresses rather than to the socket peer.
    if request.start_code().read() == LEGACY_START_CODE {
      let content = request.content();
      let opcode1 = request.opcode1().read();
      let seqnum = request.seqnum().read();
      info!(
        "received legacy 0x1200 plane opcode {opcode1:#04x} on cmc_server, content {}",
        hex::encode(content)
      );

      // The self-record's trailing 16 bytes hold two (ipv4, port, reserved)
      // records (8 bytes each): primary then secondary.
      let reply_addrs = parse_legacy_self_record(content);
      if reply_addrs.is_empty() {
        conn.respond_with_code(0x30, &[]).await;
        continue;
      }

      for dst in reply_addrs {
        conn.send(dst, LEGACY_START_CODE, seqnum, opcode1, 0x30, &[]).await;
      }
      continue;
    }

    if request.opcode2().read() == 0 {
      match request.opcode1().read() {
        REQUEST_DEVICE_ADVERTISEMENT => {
          let adv = DeviceAdvertisement {
            process_id: self_info.process_id,
            factory_device_id: self_info.factory_device_id,
            unknown1_1: 1,
            unknown2_0: 0,
            ip_address: self_info.ip_address.octets(),
            info_request_port: self_info.info_request_port,
            unknown3_0: 0,
          };
          conn.respond_with_struct(CODE_OK, adv).await;
        }
        other => {
          warn!("received unsupported CMC opcode {other:#04x}, content {}", hex::encode(request.content()));
          conn.respond_with_code(0x30, &[]).await;
        }
      }
    } else {
      warn!(
        "received unsupported CMC opcode2 {:#04x}, content {}",
        request.opcode2().read(),
        hex::encode(request.content())
      );
      conn.respond_with_code(0x30, &[]).await;
    }
  }
}

#[cfg(test)]
mod legacy_parse_tests {
  use super::parse_legacy_self_record;
  use std::net::{IpAddr, Ipv4Addr, SocketAddr};

  #[test]
  fn parses_two_records() {
    let mut content = vec![0u8; 72];
    // trailing 16 bytes: primary 192.168.50.112:8706, secondary 172.31.156.210:8706
    let tail: [u8; 16] = [
      192, 168, 50, 112, 0x22, 0x02, 0, 0,
      172, 31, 156, 210, 0x22, 0x02, 0, 0,
    ];
    content.extend_from_slice(&tail);
    let addrs = parse_legacy_self_record(&content);
    assert_eq!(addrs, vec![
      SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 50, 112)), 0x2202),
      SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 31, 156, 210)), 0x2202),
    ]);
  }

  #[test]
  fn short_content_returns_empty() {
    assert!(parse_legacy_self_record(&[0u8; 10]).is_empty());
  }

  #[test]
  fn zero_port_skipped() {
    let mut content = vec![0u8; 16];
    // port 0 in both records
    assert!(parse_legacy_self_record(&content).is_empty());
  }
}

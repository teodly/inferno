use crate::common::*;
use std::sync::Arc;

use crate::device_info::DeviceInfo;
use crate::net_utils::UdpSocketWrapper;
use crate::protocol::req_resp;
use bytebuffer::ByteBuffer;
use tokio::sync::broadcast::Receiver as BroadcastReceiver;

pub async fn run_server(self_info: Arc<DeviceInfo>, shutdown: BroadcastReceiver<()>) {
  let server = UdpSocketWrapper::new(Some(self_info.ip_address), self_info.cmc_port, shutdown).await;
  let mut conn = req_resp::Connection::new(server);
  while conn.should_work() {
    let request = match conn.recv().await {
      Some(v) => v,
      None => continue,
    };

    if request.opcode2().read() == 0 {
      match request.opcode1().read() {
        0x1001 => {
          let mut content = ByteBuffer::new();
          content.write_u16(self_info.process_id /* 0 */);
          content.write_bytes(&self_info.factory_device_id);
          content.write_u16(1);
          content.write_u16(0);
          content.write_bytes(&self_info.ip_address.octets());
          content.write_u16(self_info.info_request_port);
          content.write_u16(0);
          conn.respond(content.as_bytes()).await;
        }
        other => {
          error!("received unknown opcode1 {other:#04x}, content {}", hex::encode(request.content()));
          error!("whole packet: {:?}", hex::encode(request.into_storage()));
        }
      }
    } else {
      error!(
        "received unknown opcode2 {:#04x}, content {}",
        request.opcode2().read(),
        hex::encode(request.content())
      );
      error!("whole packet: {:?}", hex::encode(request.into_storage()));
    }
  }
}

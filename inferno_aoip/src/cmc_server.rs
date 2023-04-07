use std::sync::Arc;

use crate::device_info::DeviceInfo;
use crate::net_utils::UdpSocketWrapper;
use crate::protocol::mcast::INFO_REQUEST_PORT;
use crate::protocol::proto_cmc::PORT;
use crate::protocol::req_resp;
use bytebuffer::ByteBuffer;
use tokio::sync::broadcast::Receiver as BroadcastReceiver;

pub async fn run_server(self_info: Arc<DeviceInfo>, shutdown: BroadcastReceiver<()>) {
  let server = UdpSocketWrapper::new(Some(self_info.ip_address), PORT, shutdown);
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
          content.write_u16(0);
          content.write_bytes(&self_info.factory_device_id);
          content.write_u16(1);
          content.write_u16(0);
          content.write_bytes(&self_info.ip_address.octets());
          content.write_u16(INFO_REQUEST_PORT);
          content.write_u16(0);
          conn.respond(content.as_bytes()).await;
        }
        _ => {}
      }
    }
  }
}

use crate::common::*;
use crate::device_server::flows_tx::FlowInfo;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::flows_tx::{FlowsTransmitter, FPP_MAX, MAX_FLOWS};
use crate::protocol::flows_control::{
  parse_flow_request, parse_flow_update, FlowControlError, FlowRequest,
};
use crate::{device_info::DeviceInfo, net_utils::UdpSocketWrapper, protocol::req_resp};
use tokio::sync::broadcast::Receiver as BroadcastReceiver;

pub async fn run_server(
  self_info: Arc<DeviceInfo>,
  flows_tx: Arc<Mutex<Option<FlowsTransmitter>>>,
  shutdown: BroadcastReceiver<()>,
) {
  let server =
    UdpSocketWrapper::new(Some(self_info.ip_address), self_info.flows_control_port, shutdown).await;
  let mut conn = req_resp::Connection::new(server);
  let mut recv_buff = crate::net_utils::ReceiveBuffer::new();
  while conn.should_work() {
    let request = match conn.recv(&mut recv_buff).await {
      Some(v) => v,
      None => continue,
    };

    if request.opcode2().read() == 0 {
      match request.opcode1().read() {
        0x0100 => {
          // request flow
          let packet = request.into_storage();
          let req = match parse_flow_request(packet) {
            Ok(req) => req,
            Err(e) => {
              error!("invalid flow request ({e}): {}", hex::encode(packet));
              continue;
            }
          };
          let FlowRequest { sample_rate, bits_per_sample, fpp, rx_ip, rx_port, .. } = req;
          let channel_indices = req.channel_indices;
          info!(
            "{} requesting flow {} of channel indices {channel_indices:?} at {sample_rate}Hz {bits_per_sample}bit {fpp} fpp to {rx_ip}:{rx_port}",
            req.rx_hostname, req.rx_flow_name
          );
          if channel_indices.iter().flatten().any(|&chi| chi >= self_info.tx_channels.len()) {
            error!("too large channel number, returning error");
            conn.respond_with_code(0x0302u16 /* ??? TODO */, &[]).await;
            continue;
          }
          if sample_rate != self_info.sample_rate {
            error!("sample rate mismatch, returning error");
            conn.respond_with_code(FlowControlError::SampleRateMismatch as u16, &[]).await;
            continue;
          }
          // The flow's sample width comes from the network: 20 bits used to
          // become 2 bytes (16-bit samples) and 33 bits 4, silently
          // sending a format the receiver never asked for.
          if !matches!(bits_per_sample, 16 | 24 | 32) {
            error!("unsupported bit depth {bits_per_sample}, returning error");
            conn.respond_with_code(0x0302u16 /* TODO */, &[]).await;
            continue;
          }
          if fpp > FPP_MAX {
            error!("too large fpp, returning error");
            conn.respond_with_code(0x0302u16 /* TODO */, &[]).await;
            continue;
          }
          let flow_info = FlowInfo {
            rx_hostname: Some(req.rx_hostname.to_owned()),
            rx_flow_name: Some(req.rx_flow_name.to_owned()),
            dst_addr: rx_ip,
            dst_port: rx_port,
            local_channel_indices: channel_indices,
          };
          let result = match flows_tx.lock().await.as_mut() {
            Some(tx) => {
              tx.add_flow(flow_info, fpp as usize, (bits_per_sample / 8) as usize, None, false).await
            }
            None => {
              error!("flow requested but this device has no transmitter running");
              conn.respond_with_code(FlowControlError::TooManyTXFlows as u16, &[]).await;
              continue;
            }
          };
          match result {
            Ok((_flow_index, handle)) => {
              conn.respond(&handle).await;
            }
            Err(e) => {
              error!("adding flow failed: {e:?}");
              conn.respond_with_code(FlowControlError::TooManyTXFlows as u16, &[]).await;
            }
          }
        }
        0x0101 => {
          // stop flow
          let handle = if let Ok(handle) = request.content().try_into() {
            handle
          } else {
            error!("packet too short: {}", hex::encode(request.content()));
            continue;
          };
          let removed = match flows_tx.lock().await.as_mut() {
            Some(tx) => tx.remove_flow(handle).await.is_ok(),
            None => false,
          };
          if removed {
            info!("stopped flow {handle:?}");
            conn.respond(&[]).await;
          } else {
            warn!("received stop flow request for unknown handle {handle:?}");
            conn.respond_with_code(FlowControlError::FlowNotFound as u16, &[]).await;
          }
        }
        0x0102 => {
          // update flow
          let (handle, channel_indices) = match parse_flow_update(request.content()) {
            Ok(v) => v,
            Err(e) => {
              error!("invalid update flow request ({e}): {}", hex::encode(request.content()));
              continue;
            }
          };
          if channel_indices.iter().flatten().any(|&chi| chi >= self_info.tx_channels.len()) {
            error!("update flow: too large channel number, returning error");
            conn.respond_with_code(0x0302u16 /* ??? TODO */, &[]).await;
            continue;
          }
          let updated = match flows_tx.lock().await.as_mut() {
            Some(tx) => tx.set_channels(handle, channel_indices.clone()).await.is_ok(),
            None => false,
          };
          if updated {
            info!("set channels {channel_indices:?} in flow {handle:?}");
            conn.respond(&[]).await;
          } else {
            warn!("received update flow request for unknown handle {handle:?}");
            conn.respond_with_code(FlowControlError::FlowNotFound as u16, &[]).await;
          }
        }

        x => {
          warn!("received unsupported flows-control opcode {x:#04x}, content {}", hex::encode(request.content()));
          conn.respond_with_code(0x30, &[]).await;
        }
      }
    } else {
      warn!(
        "received unsupported flows-control opcode2 {:#04x}, content {}",
        request.opcode2().read(),
        hex::encode(request.content())
      );
      conn.respond_with_code(0x30, &[]).await;
    }
  }
  if let Some(tx) = flows_tx.lock().await.as_mut() {
    tx.shutdown().await;
  }
}

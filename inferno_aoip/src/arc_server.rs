use crate::byte_utils::*;
use crate::channels_subscriber::ChannelsSubscriber;

use crate::device_info::DeviceInfo;
use crate::net_utils::UdpSocketWrapper;
use crate::protocol::proto_arc::PORT;
use crate::protocol::req_resp;
use crate::protocol::req_resp::HEADER_LENGTH;
use bytebuffer::{ByteBuffer, Endian};
use log::{error, info, trace};
use std::{cmp::min, sync::Arc};
use tokio::sync::broadcast::Receiver as BroadcastReceiver;
use tokio::sync::watch;

pub async fn run_server(
  self_info: Arc<DeviceInfo>,
  mut channels_sub_rx: watch::Receiver<Option<Arc<ChannelsSubscriber>>>,
  shutdown: BroadcastReceiver<()>,
) {
  let mut subscriber = None;
  let server = UdpSocketWrapper::new(Some(self_info.ip_address), PORT, shutdown);
  let mut conn = req_resp::Connection::new(server);
  while conn.should_work() {
    let request = match conn.recv().await {
      Some(v) => v,
      None => continue,
    };

    if channels_sub_rx.has_changed().unwrap_or(false) {
      subscriber = channels_sub_rx.borrow_and_update().clone();
    }

    if request.opcode2().read() == 0 {
      match request.opcode1().read() {
        0x1000 => {
          let txnum = self_info.tx_channels.len() as u16;
          let rxnum = self_info.rx_channels.len() as u16;
          // almost the same for all devices
          conn
            .respond(&[
              0x00, // was 0x05
              0x00, // was 0xf9
              H(txnum),
              L(txnum),
              H(rxnum),
              L(rxnum),
              0x00,
              0x00, // was 4
              0x00,
              0x00, // was 4
              0x00,
              0x00, // was 8
              0x00,
              0x00, // was 2
              0x00, // max receive flows MSB
              0x08, // max receive flows LSB
              0x00,
              0x00, // was 4, 0 also occurs in some devices
              0x00,
              0x00, // was 1
              0x00,
              0x01, // if 0, DC doesn't recognize RX channels
              0x00,
              0x00,
              0x00,
              0x00,
              0x00,
              0x00,
              0x00,
              0x00,
              0x00,
              0x00,
              0x00,
              0x00,
            ])
            .await;
        }
        0x1102 => {
          // identical for all low channels count devices
          let content = [0u8; 94];
          conn
            .respond(&content)
            .await;
        }
        0x1002 => {
          // device name (used by network-audio-controller)
          let mut buff = ByteBuffer::new();
          buff.write_bytes(self_info.friendly_hostname.as_bytes());
          buff.write_u8(0);
          conn.respond(buff.as_bytes()).await;
        }
        0x1003 => {
          // hostname, board name, factory names:
          let mut content = [
            0x00, 0x00, 0x00, 0 /*was 0x14*/, 0x00, 0 /*was 0x20*/, /* offset of board name: */ 0x00, 0x70,
            /* offset of :70{2,5} (revision string?) */ 0x00, 0x90, 0 /*was 5*/, 0x00,
            /* offset of friendly host name: */ 0x00, 0x30,
            /* offset of factory host name: */ 0x00, 0x50,
            /* offset of friendly host name again: */ 0x00, 0x30, 0x00, 0x00, 0x00, 0x00,
            0 /*was 4*/, 0x00, 0x00, 0x00, 0 /*was 4*/, 0x00, 0 /*was 2*/, /* for :705, 1 for :702 */
            0x00, /* start code: */ 0x27, 0x29, 0 /*was 2*/, 0 /*was 4*/,
            /* opcode of some other request: */ 0x11, 0x02, 0 /*was 0x10*/, 0 /*was 4*/,
            /* friendly host name: */
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, /* factory host name: */
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, /* board name: */
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, /* :70{2,5} */
            0x3a, 0x37, 0x30, 0x35, 0x00,
          ];
          let HOST_NAMES_MAXLEN = 0x20 - 1;
          let BOARD_NAME_MAXLEN = 23;
          write_str_to_buffer(
            &mut content,
            0x30 - HEADER_LENGTH,
            HOST_NAMES_MAXLEN,
            &self_info.friendly_hostname,
          );
          write_str_to_buffer(
            &mut content,
            0x50 - HEADER_LENGTH,
            HOST_NAMES_MAXLEN,
            &self_info.factory_hostname,
          );
          write_str_to_buffer(
            &mut content,
            0x70 - HEADER_LENGTH,
            BOARD_NAME_MAXLEN,
            &self_info.board_name,
          );
          conn.respond(&content).await;
        }

        0x3000 => {
          // Dante Receivers names and subscriptions:
          /*respond(&[0x02, 0x02,
          0x00, 0x01, 0x00, 0x06,
          /* binary descriptor offset: */ 0x00, 0x34,
          /* tx channel name offset: */ 0x00, 0x00,
          /* tx device hostname offset: */ 0x00, 0x00,
          /* local rx channel name offset: */ 0x00, 0x44,
          /* subscription status, 1,1,0,9 if subscribed, 0,0,0,1 if tx not found */ 0x00, 0x00, 0x00, 0x00,
          0x00, 0x00, 0x00, 0x00,
          0x00, 0x02, 0x00, 0x06,
          /* binary descriptor offset: */ 0x00, 0x34,
          0x00, 0x00, 0x00, 0x00,
          /* local channel name offset: */ 0x00, 0x46,
          0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
          /* binary descriptor: */
          0x00, 0x00, 0xbb, 0x80, 0x01, 0x01, 0x00, 0x18, 0x04, 0x00, 0x00, 0x18, 0x00, 0x18, 0x00, 0x04,
          /* channel names follow: */
          0x31, 0x00, 0x32, 0x00]);*/
          let content = request.content();
          let start_index = make_u16(content[2], content[3]) - 1;
          let remaining = if subscriber.is_some() { self_info.rx_channels.len().saturating_sub(start_index as usize) } else { 0 };
          let limit = 16;
          let in_this_response = min(limit, remaining);
          trace!("returning {in_this_response} rx channels starting with index {start_index}");
          let mut response = ByteBuffer::new();
          response.set_endian(Endian::BigEndian);
          //response.write_u8(self_info.rx_channels.len() as u8); // total number of channels(?)
          response.write_u8(in_this_response as u8);
          response.write_u8(in_this_response as u8); // number of channels in this response(?)
          let common_descr_offset = HEADER_LENGTH + 2 + in_this_response * 20;
          let strings_offset = common_descr_offset + 16;
          let mut strings = ByteBuffer::new();
          for (i, ch) in self_info
            .rx_channels
            .iter()
            .enumerate()
            .skip(start_index as usize)
            .take(in_this_response)
          {
            response.write_u16((i + 1) as u16); // channel id
            response.write_u16(6); // ???
            response.write_u16(common_descr_offset as u16);
            let status = subscriber.as_ref().unwrap().channel_status(i);
            let (tx_ch_offset, tx_host_offset) = match &status {
              None => (0, 0),
              Some(status) => {
                let tx_ch_offset = (strings.get_wpos() + strings_offset) as u16;
                strings.write_bytes(status.tx_channel_name.as_bytes());
                strings.write_u8(0);
                let tx_host_offset = (strings.get_wpos() + strings_offset) as u16;
                strings.write_bytes(status.tx_hostname.as_bytes());
                strings.write_u8(0);
                (tx_ch_offset, tx_host_offset)
              }
            };
            response.write_u16(tx_ch_offset);
            response.write_u16(tx_host_offset);
            response.write_u16((strings.get_wpos() + strings_offset) as u16);
            strings.write_bytes(ch.friendly_name.as_bytes());
            strings.write_u8(0);
            let status_value: u32 = match &status {
              None => 0,
              Some(ss) => ss.status as u32,
            };
            response.write_u32(status_value); // subscription status, TODO. 0x01010009 if subscribed currently, 0x00000001 if not found but remembers subscription or in progress
            response.write_u32(0); // ???
          }
          response.write_u32(self_info.sample_rate);
          response.write_bytes(&[
            0x01,
            0x01,
            0x00,
            0x18,
            0x04,
            0x00,
            0x00,
            0x18,
            0x00,
            0x18,
            0x00,
            self_info.pcm_type,
          ]);
          response.write_bytes(strings.as_bytes());
          let code = if remaining > in_this_response { 0x8112 } else { 1 };
          conn.respond_with_code(code, response.as_bytes()).await;
        }

        0x2000 => {
          // Dante Transmitters default names:
          /*respond(&[
          /* number of channels, repeated 2x ??? */ 0x04, 0x04,
          0x00, 0x01, 0x00, 0x07, /* offset of binary descriptor: */ 0x00, 0x2c,
          /* offset of channel 1 name: */ 0x00, 0x3c,
          0x00, 0x02, 0x00, 0x07, 0x00, 0x2c,
          /* offset of channel 2 name: */ 0x00, 0x48,
          0x00, 0x03, 0x00, 0x07, 0x00, 0x2c,
          /* offset of channel 3 name: */ 0x00, 0x54,
          0x00, 0x04, 0x00, 0x07, 0x00, 0x2c,
          /* offset of channel 4 name: */ 0x00, 0x60,
          /* binary descriptor: */
          0x00, 0x00, 0xbb, 0x80, 0x01, 0x01, 0x00, 0x18, 0x04, 0x00, 0x00, 0x18, 0x00, 0x18,
          /* PCM type: */ 0x00, 0x04,
          /* 0x3C: channel 1 */
          0x41, 0x6e, 0x61, 0x6c, 0x6f, 0x67, 0x20, 0x49, 0x6e, 0x20, 0x31, 0x00,
          0x41, 0x6e, 0x61, 0x6c, 0x6f, 0x67, 0x20, 0x49, 0x6e, 0x20, 0x32, 0x00,
          0x41, 0x6e, 0x61, 0x6c, 0x6f, 0x67, 0x20, 0x49, 0x6e, 0x20, 0x33, 0x00,
          0x41, 0x6e, 0x61, 0x6c, 0x6f, 0x67, 0x20, 0x49, 0x6e, 0x20, 0x34, 0x00]);*/
          let content = request.content();
          let start_index = make_u16(content[2], content[3]) as usize - 1;
          let remaining = self_info.tx_channels.len() - start_index as usize;
          let limit = 16;
          let in_this_response = min(limit, remaining);
          trace!("returning {in_this_response} tx channels default names starting with index {start_index}");
          
          let channels_names_total: usize =
            self_info.tx_channels.iter().skip(start_index).take(in_this_response).map(|ch| ch.factory_name.len() + 1).sum();
          let mut content = vec![0; 2 + in_this_response * 8 + 16 + channels_names_total];
          content[0] = in_this_response as u8;
          content[1] = in_this_response as u8;
          let mut ch_descr_offset: u16 = HEADER_LENGTH as u16 + 2;
          let common_descr_offset: u16 = ch_descr_offset + in_this_response as u16 * 8;

          let mut descr = ByteBuffer::new();
          descr.write_u32(self_info.sample_rate);
          descr.write_bytes(&[
            0x01,
            0x01,
            0x00,
            0x18,
            0x04,
            0x00,
            0x00,
            0x18,
            0x00,
            0x18,
            0,
            self_info.pcm_type,
          ]);
          content[common_descr_offset as usize - HEADER_LENGTH as usize..][..16]
            .clone_from_slice(descr.as_bytes());
          let mut name_offset: u16 = common_descr_offset + 16;
          for (i, ch) in self_info
            .tx_channels
            .iter()
            .enumerate()
            .skip(start_index as usize)
            .take(in_this_response) {
            let channel_id = (i + 1) as u16;
            content[ch_descr_offset as usize - HEADER_LENGTH..][..8].clone_from_slice(&[
              H(channel_id),
              L(channel_id),
              0,
              7,
              H(common_descr_offset),
              L(common_descr_offset),
              H(name_offset),
              L(name_offset),
            ]);
            write_str_to_buffer(
              &mut content,
              (name_offset as usize) - HEADER_LENGTH,
              ch.factory_name.len(),
              &ch.factory_name,
            );
            ch_descr_offset += 8;
            name_offset += ch.factory_name.len() as u16 + 1;
          }
          let code = if remaining > in_this_response { 0x8112 } else { 1 };
          conn.respond_with_code(code, &content).await;
          // TODO rewrite this with ByteBuffer
        }
        0x2010 => {
          // Dante Transmitters user-specified names:
          /*respond(&[0x04, 0x04, 0x00, 0x01, 0x00, 0x01, 0x00, 0x2c, 0x00, 0x02, 0x00, 0x02, 0x00, 0x38, 0x00, 0x03, 0x00, 0x03, 0x00, 0x44, 0x00, 0x04, 0x00, 0x04, 0x00, 0x4d, 0x00, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0x4f, 0x64, 0x62, 0x69, 0x6f, 0x72, 0x6e, 0x69, 0x6b, 0x2d, 0x4c, 0x00, 0x4f, 0x64, 0x62, 0x69, 0x6f, 0x72, 0x6e, 0x69, 0x6b, 0x2d, 0x52, 0x00, 0x49, 0x6e, 0x74, 0x65, 0x72, 0x63, 0x6f, 0x6d, 0x00, 0x34, 0x00]);*/
          let content = request.content();
          let start_index = make_u16(content[2], content[3]) as usize - 1;
          let remaining = self_info.tx_channels.len() - start_index as usize;
          let limit = 16;
          let in_this_response = min(limit, remaining);
          trace!("returning {in_this_response} tx channels friendly names starting with index {start_index}");

          let mut response = ByteBuffer::new();
          response.write_u8(in_this_response as u8);
          response.write_u8(in_this_response as u8);
          let mut strings = ByteBuffer::new();
          let strings_offset = HEADER_LENGTH + 2 + in_this_response * 6 + 4;
          for (i, ch) in self_info
            .tx_channels
            .iter()
            .enumerate()
            .skip(start_index as usize)
            .take(in_this_response) {
            let channel_id = (i + 1) as u16;
            response.write_u16(channel_id);
            response.write_u16(channel_id);
            response.write_u16((strings.get_wpos() + strings_offset) as u16);
            strings.write_bytes(ch.friendly_name.as_bytes());
            strings.write_u8(0);
          }
          response.write_u32(0); // ??? used to be 0,0,0,1, or 1,1,0,9, maybe random memory fragments???
          response.write_bytes(strings.as_bytes());

          let code = if remaining > in_this_response { 0x8112 } else { 1 };
          conn.respond_with_code(code, response.as_bytes()).await;
        }
        0x2200 => {
          // Destinations of this device's Transmitters
        }

        0x2201 => {
          // Create multicast TX flow
        }
        0x3200 => {
          // ???
          conn.respond(&[0 /*was 2*/, 0x00, 0x00, 0x00, 0x00, 0x00]).await;
        }

        0x1100 => {
          // ???
          // looks like something dependent on active connections
          let content = [0u8; 110]; /*[
            0x12, 0x12, 0x02, 0x01, 0x00, 0x01, 0x82, 0x04, 0x00, 0x54, 0x82, 0x05, 0x00, 0x58,
            0x02, 0x10, 0x00, 0x10, 0x02, 0x11, 0x00, 0x10, 0x00, 0x00, 0x82, 0x18, 0x00, 0x00,
            0x82, 0x19, 0x83, 0x01, 0x00, 0x5c, 0x83, 0x02, 0x00, 0x60, 0x83, 0x06, 0x00, 0x64,
            0x03, 0x10, 0x00, 0x10, 0x03, 0x11, 0x00, 0x10, 0x03, 0x03, 0x00, 0x02, 0x80, 0x21,
            0x00, 0x68, 0x00, 0x00, 0x00, 0xf0, 0x00, 0x00, 0x80, 0x60, 0x00, 0x22, 0x00, 0x01,
            0x00, 0x00, 0x00, 0x63, /* 1000000: */ 0x00, 0x0f, 0x42, 0x40, 0x00, 0x0f, 0x42,
            0x40, 0x00, 0x0f, 0x42, 0x40, 0x01, 0x35, 0xf1, 0xb4, 0x00, 0x0f, 0x42, 0x40, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00,
          ];*/
          conn
            .respond(&content)
            .await;
        }
        0x3300 => {
          // ???
          //conn.respond(&[0x38, 0x00, 0x38, 0xfd, 0x38, 0xfe, 0x38, 0xff]).await;
          conn.respond(&[0u8; 8]).await;
        }

        0x3010 => {
          // subscribe (connect our receiver to remote transmitter)
          // or unsubscribe if tx_*_offset is 0
          if let Some(channels_recv) = &subscriber {
            let c_whole = request.content();
            let count = c_whole[1] as usize;
            for i in 0..count {
              let c = &c_whole[2 + i * 6..];
              let local_channel = make_u16(c[0], c[1]);
              let tx_channel_offset = make_u16(c[2], c[3]) as usize;
              let tx_hostname_offset = make_u16(c[4], c[5]) as usize;
              let local_channel_index = (local_channel - 1) as usize;
              if tx_channel_offset > 0 && tx_hostname_offset > 0 {
                let str_or_none = |offset| match offset {
                  _ if offset < HEADER_LENGTH => None,
                  v => match read_0term_str_from_buffer(&c_whole, v - HEADER_LENGTH) {
                    Ok(s) => Some(s),
                    Err(e) => {
                      error!("failed to decode string: {e:?}");
                      None
                    }
                  },
                };
                let tx_channel_name = str_or_none(tx_channel_offset);
                let tx_hostname = str_or_none(tx_hostname_offset);
                info!(
                  "connection requested: {} <- {:?} @ {:?}",
                  local_channel, tx_channel_name, tx_hostname
                );
                if tx_channel_name.is_some() && tx_hostname.is_some() {
                  channels_recv
                    .subscribe(local_channel_index, tx_channel_name.unwrap(), tx_hostname.unwrap())
                    .await;
                } else {
                  error!(
                    "couldn't read tx names from subscription request: {}",
                    hex::encode(&c_whole)
                  );
                }
              } else {
                info!("disconnect requested: local channel {}", local_channel);
                channels_recv.unsubscribe(local_channel_index).await;
              }
            }
            conn.respond(&[]).await;
          }
        }
        x => {
          error!("received unknown opcode1 {x:#04x}, content {}", hex::encode(request.content()));
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

use super::channels_subscriber::ChannelsSubscriber;
use super::saved_settings::{is_valid_channel_name, SavedChannelsSettings};
use super::tx_multicasts::TransmitMulticasts;
use crate::mdns_client::MdnsClient;
use crate::{byte_utils::*, net_utils};

use super::flows_rx::MAX_FLOWS as MAX_RX_FLOWS;
use super::flows_tx::{
  validate_flow_layout, FlowsTransmitter, MAX_CHANNELS_IN_FLOW, MAX_FLOWS as MAX_TX_FLOWS,
};
use super::flows_tx::{FlowInfo as TXFlowInfo, FPP_MAX_ADVERTISED};
use super::mdns_server::DeviceMDNSResponder;
use crate::device_info::DeviceInfo;
use crate::net_utils::UdpSocketWrapper;
use crate::protocol::mcast::make_channel_change_notification;
use crate::protocol::mcast::MulticastMessage;
use crate::protocol::proto_arc::*;
use crate::protocol::req_resp::HEADER_LENGTH;
use crate::protocol::req_resp::{self, CODE_OK};
use crate::state_storage::StateStorage;
use crate::utils::LogAndForget;
use binary_serde::recursive_array::RecursiveArray as _;
use binary_serde::BinarySerde;
use bytebuffer::{ByteBuffer, Endian};
use itertools::Itertools;
use log::{error, info, trace, warn};
use rand::{thread_rng, Rng as _};
use std::net::Ipv4Addr;
use std::sync::RwLock;
use std::{cmp::min, sync::Arc};
use tokio::sync::broadcast::Receiver as BroadcastReceiver;
use tokio::sync::mpsc::Sender;
use tokio::sync::{watch, Mutex};

pub async fn run_server(
  self_info: Arc<DeviceInfo>,
  state_storage: Arc<StateStorage>,
  mdns_server: Arc<DeviceMDNSResponder>,
  mcast: Sender<MulticastMessage>,
  mut channels_sub_rx: watch::Receiver<Option<Arc<ChannelsSubscriber>>>,
  flows_tx: Arc<Mutex<Option<FlowsTransmitter>>>,
  tx_multicasts: Arc<Mutex<Option<TransmitMulticasts>>>,
  shutdown: BroadcastReceiver<()>,
) {
  let mut subscriber = None;
  let mut saved_channels = SavedChannelsSettings::load(state_storage, self_info.clone());
  let server = UdpSocketWrapper::new(Some(self_info.ip_address), self_info.arc_port, shutdown).await;
  let mut conn = req_resp::Connection::new(server);
  let mut recv_buff = net_utils::ReceiveBuffer::new();
  while conn.should_work() {
    let request = match conn.recv(&mut recv_buff).await {
      Some(v) => v,
      None => continue,
    };

    if channels_sub_rx.has_changed().unwrap_or(false) {
      subscriber = channels_sub_rx.borrow_and_update().clone();
    }

    if request.opcode2().read() == 0 {
      match request.opcode1().read() {
        channels_and_flows_count::OPCODE => {
          let total_channels_wtf = self_info.tx_channels.len() + self_info.rx_channels.len(); // ??? not actually total number of channels but in some devices it is
          let response = channels_and_flows_count::Response {
            unknown1_0: 0,
            flags2: channels_and_flows_count::Flags2 {
              supports_tx_channel_rename: true,
              supports_tx_multicast: true,
              ..Default::default()
            },
            tx_channels_count: self_info.tx_channels.len().try_into().unwrap(),
            rx_channels_count: self_info.rx_channels.len().try_into().unwrap(),
            unknown2_4: 4, // or 1
            max_channels_in_flow: MAX_CHANNELS_IN_FLOW
              .min(self_info.tx_channels.len().try_into().unwrap()), // or 8
            unknown4_8: 8,
            max_tx_flows: MAX_TX_FLOWS.try_into().unwrap(),
            max_rx_flows: MAX_RX_FLOWS.try_into().unwrap(),
            unknown5_total_channels: total_channels_wtf.try_into().unwrap(),
            unknown6_1: 1,
            unknown7_1: 1,
            unknown8_0: [0; 6],
          };
          conn.respond_with_struct(CODE_OK, response).await;
        }

        GET_DEVICE_NAME_OPCODE => {
          // device name (used by network-audio-controller)
          let mut buff = ByteBuffer::new();
          buff.write_bytes(self_info.friendly_hostname.as_bytes());
          buff.write_u8(0);
          conn.respond(buff.as_bytes()).await;
        }

        get_device_names::OPCODE => {
          let mut bytes = ByteBuffer::new();
          let strings_offset = HEADER_LENGTH;
          bytes.write_bytes(&[0u8; get_device_names::ResponseHeader::SERIALIZED_SIZE]);
          let friendly_hostname_offset = (bytes.get_wpos() + strings_offset).try_into().unwrap();
          bytes.write_bytes(self_info.friendly_hostname.as_bytes());
          bytes.write_u8(0);
          let factory_hostname_offset = (bytes.get_wpos() + strings_offset).try_into().unwrap();
          bytes.write_bytes(self_info.factory_hostname.as_bytes());
          bytes.write_u8(0);
          let board_name_offset = (bytes.get_wpos() + strings_offset).try_into().unwrap();
          bytes.write_bytes(self_info.board_name.as_bytes());
          bytes.write_u8(0);
          let revision_string_offset = (bytes.get_wpos() + strings_offset).try_into().unwrap();
          bytes.write_bytes(b":705\0");

          let response = get_device_names::ResponseHeader {
            board_name_offset,
            revision_string_offset,
            friendly_hostname_offset1: friendly_hostname_offset,
            factory_hostname_offset,
            friendly_hostname_offset2: friendly_hostname_offset,
            start_code: 0x2729,
            unknown_opcode_1102: 0x1102,
            ..Default::default()
          };
          bytes.set_wpos(0);
          bytes.write_bytes(response.binary_serialize_to_array(binary_serde::Endianness::Big).as_slice());
          conn.respond(&bytes.as_bytes()).await;
        }

        get_receive_channels::OPCODE => {
          // Dante Receivers names and subscriptions:

          let mut common_descriptor_offset: u16 = 0;
          paginate_respond(
            &mut conn,
            request.content(),
            if subscriber.is_some() {
              self_info.rx_channels.len().min(RX_CHANNELS_PAGE_SIZE).try_into().unwrap()
            } else {
              0
            },
            self_info.rx_channels.iter().enumerate(),
            |(channel_index, ch), bytes| {
              if common_descriptor_offset == 0 {
                let descr = CommonChannelsDescriptor::new(&self_info);
                common_descriptor_offset = bytes.get_wpos().try_into().unwrap();
                bytes
                  .write_bytes(descr.binary_serialize_to_array(binary_serde::Endianness::Big).as_slice());
              }
              let status = subscriber.as_ref().unwrap().channel_status(channel_index);
              let (tx_channel_name_offset, tx_hostname_offset) = match &status {
                None => (0, 0),
                Some(status) => (
                  write_0term_str_to_bytebuffer(bytes, &status.tx_channel_name),
                  write_0term_str_to_bytebuffer(bytes, &status.tx_hostname),
                ),
              };
              let status_value: u32 = match &status {
                None => 0,
                Some(ss) => ss.status as u32,
              };
              Some(get_receive_channels::ChannelDescriptor {
                channel_id: (channel_index + 1).try_into().unwrap(),
                unknown1_6: 6,
                common_descriptor_offset,
                tx_channel_name_offset,
                tx_hostname_offset,
                friendly_name_offset: write_0term_str_to_bytebuffer(
                  bytes,
                  &ch.friendly_name.read().unwrap(),
                ),
                subscription_status: status_value,
                unknown2_0: 0,
              })
            },
          )
          .await;
        }

        get_transmit_channels::OPCODE => {
          // Dante Transmitters default names:
          let mut common_descriptor_offset: u16 = 0;
          paginate_respond(
            &mut conn,
            request.content(),
            self_info.tx_channels.len().min(TX_CHANNELS_PAGE_SIZE).try_into().unwrap(),
            self_info.tx_channels.iter().enumerate(),
            |(channel_index, ch), bytes| {
              if common_descriptor_offset == 0 {
                let descr = CommonChannelsDescriptor::new(&self_info);
                common_descriptor_offset = bytes.get_wpos().try_into().unwrap();
                bytes
                  .write_bytes(descr.binary_serialize_to_array(binary_serde::Endianness::Big).as_slice());
              }
              Some(get_transmit_channels::ChannelDescriptor {
                channel_id: (channel_index + 1).try_into().unwrap(),
                unknown1_7: 7,
                common_descriptor_offset,
                name_offset: write_0term_str_to_bytebuffer(bytes, &ch.factory_name),
              })
            },
          )
          .await;
        }

        get_transmit_channels_friendly_names::OPCODE => {
          // Dante Transmitters user-specified names:
          let mut wrote = false;
          paginate_respond(
            &mut conn,
            request.content(),
            self_info.tx_channels.len().min(TX_CHANNELS_PAGE_SIZE).try_into().unwrap(),
            self_info.tx_channels.iter().enumerate(),
            |(channel_index, ch), bytes| {
              if !wrote {
                bytes.write_u32(0);
                wrote = true;
              }
              let channel_id = (channel_index + 1).try_into().unwrap();
              Some(get_transmit_channels_friendly_names::ChannelDescriptor {
                channel_id_1: channel_id,
                channel_id_2: channel_id,
                friendly_name_offset: write_0term_str_to_bytebuffer(
                  bytes,
                  &ch.friendly_name.read().unwrap(),
                ),
              })
            },
          )
          .await;
        }

        rename_tx_channels::OPCODE => {
          let content = request.content();
          let mut renamed_ids = deserialize_items::<rename_tx_channels::SingleChannelRenameRequest>(
            content,
          )
          .filter_map(|rename| {
            let channel_id = rename.channel_id;
            let name_offset = rename.new_name_offset.saturating_sub(HEADER_LENGTH as _) as usize;
            if channel_id == 0 || name_offset == 0 {
              return None;
            }
            match read_0term_str_from_buffer(content, name_offset) {
              Ok(new_name) if !is_valid_channel_name(new_name) => {
                error!("refusing to rename TX channel id {channel_id} to invalid name {new_name:?}");
                None
              }
              Ok(new_name) => {
                let index = (channel_id - 1) as usize;
                if self_info.tx_channels.get(index).is_some_and(|c| c.fixed_name) {
                  error!("refusing to rename TX channel id {channel_id}: its name is fixed");
                  None
                } else if index < self_info.tx_channels.len() {
                  info!("renaming TX channel id {channel_id} to {new_name}");
                  mdns_server.remove_tx_channel(index);
                  saved_channels.rename_tx_channel(index, new_name.to_owned());
                  mdns_server.add_tx_channel(index);
                  Some(channel_id)
                } else {
                  error!("got rename TX channel request with invalid channel number {channel_id}");
                  None
                }
              }
              Err(e) => {
                error!("could not read new channel name from packet: {e:?}");
                None
              }
            }
          });
          let renamed_anything = renamed_ids.next().is_some();
          renamed_ids.for_each(drop); // consume the whole iterator

          if renamed_anything {
            conn.respond_with_code(1, &[0, 0]).await;
            // sometimes it is [0, 1, 0, 0, H(channel_id), L(channel_id)], but it doesn't look necessary
          } else {
            conn.respond_with_code(0xFFFF /* TODO: really? */, &[]).await;
          }
        }
        rename_rx_channels::OPCODE => {
          let content = request.content();
          let mut renamed_any = false;
          let renamed_indices = deserialize_items::<rename_rx_channels::SingleChannelRenameRequest>(
            content,
          )
          .filter_map(|rename| {
            let channel_id = rename.channel_id;
            let name_offset = rename.new_name_offset.saturating_sub(HEADER_LENGTH as _) as usize;
            if channel_id == 0 || name_offset == 0 {
              return None;
            }
            match read_0term_str_from_buffer(content, name_offset) {
              Ok(new_name) if !is_valid_channel_name(new_name) => {
                error!("refusing to rename RX channel id {channel_id} to invalid name {new_name:?}");
                None
              }
              Ok(new_name) => {
                let index = (channel_id - 1) as usize;
                if self_info.rx_channels.get(index).is_some_and(|c| c.fixed_name) {
                  error!("refusing to rename RX channel id {channel_id}: its name is fixed");
                  None
                } else if index < self_info.rx_channels.len() {
                  info!("renaming RX channel id {channel_id} to {new_name}");
                  saved_channels.rename_rx_channel(index, new_name.to_owned());
                  renamed_any = true;
                  Some(index)
                } else {
                  error!("got rename RX channel request with invalid channel number {channel_id}");
                  None
                }
              }
              Err(e) => {
                error!("could not read new channel name from packet: {e:?}");
                None
              }
            }
          });
          mcast.send(make_channel_change_notification(renamed_indices)).await.log_and_forget();
          conn
            .respond_with_code(
              if renamed_any {
                1
              } else {
                0xFFFF /* TODO */
              },
              &[],
            )
            .await;
        }
        query_tx_flows::OPCODE => {
          // query TX flows
          let content = request.content();
          let (code, response) = paginate_make_response(
            &mut conn,
            content,
            MAX_TX_FLOWS.min(16).try_into().unwrap(),
            flows_tx
              .lock()
              .await
              .as_ref()
              .map(|tx| tx.get_flows_info())
              .unwrap_or(&vec![])
              .iter()
              .enumerate(),
            |(flow_index, flow_opt), bytes| -> Option<u16> {
              flow_opt.as_ref().map(|flow_info| -> u16 {
                let flow_id = flow_index + 1;
                let flow_name = format!("{}_{}", flow_id, self_info.process_id);
                let local_tx_flow_name_offset = write_0term_str_to_bytebuffer(bytes, &flow_name);
                let remote_hostname_offset =
                  write_0term_str_or_0_to_bytebuffer(bytes, flow_info.rx_hostname.as_deref());
                let remote_rx_flow_name_offset =
                  write_0term_str_or_0_to_bytebuffer(bytes, flow_info.rx_flow_name.as_deref());
                let is_multicast = remote_hostname_offset == 0 && remote_rx_flow_name_offset == 0;

                align_wpos(bytes, 4);
                let receiver_socket_descriptor_offset = bytes.get_wpos().try_into().unwrap();
                bytes.write_bytes(
                  DestinationSocketDescriptor {
                    unknown1_8002: 0x8002,
                    port: flow_info.dst_port,
                    addr: flow_info.dst_addr.octets(),
                  }
                  .binary_serialize_to_array(binary_serde::Endianness::Big)
                  .as_slice(),
                );

                let names_descriptor_offset = bytes.get_wpos().try_into().unwrap();
                bytes.write_bytes(
                  query_tx_flows::NamesDescriptor {
                    unknown1_a00: 0xa00,
                    unknown2_1: 1,
                    remote_hostname_offset,
                    remote_rx_flow_name_offset,
                    unknown3_10: 0x10,
                    local_tx_flow_name_offset,
                    ..Default::default()
                  }
                  .binary_serialize_to_array(binary_serde::Endianness::Big)
                  .as_slice(),
                );

                let main_descriptor_offset = bytes.get_wpos();
                bytes.write_bytes(
                  query_tx_flows::FlowDescriptorHeader {
                    flow_id: flow_id.try_into().unwrap(),
                    flow_type: if is_multicast { 2 } else { 0x11 },
                    sample_rate: self_info.sample_rate,
                    unknown1_0: 0,
                    bits_per_sample: self_info.bits_per_sample.into(),
                    unknown2_1: 1,
                    channels_count: flow_info.local_channel_indices.len().try_into().unwrap(),
                    receiver_socket_descriptor_offset,
                  }
                  .binary_serialize_to_array(binary_serde::Endianness::Big)
                  .as_slice(),
                );

                for ch in &flow_info.local_channel_indices {
                  bytes.write_u16(ch.map(|i| i + 1).unwrap_or(0).try_into().unwrap());
                }

                bytes.write_bytes(
                  query_tx_flows::FlowDescriptorFooter { names_descriptor_offset }
                    .binary_serialize_to_array(binary_serde::Endianness::Big)
                    .as_slice(),
                );

                main_descriptor_offset.try_into().unwrap()
              })
            },
          );
          conn.respond_with_code(code, &response).await;
        }
        create_multicast_tx_flow::OPCODE => {
          // Create multicast TX flow
          let content = request.content();
          let mut flow_ids = vec![];
          for descr_offset in deserialize_items::<u16>(content) {
            let (descr, channel_ids) =
              match create_multicast_tx_flow::parse_descriptor(content, descr_offset.into()) {
                Some(v) => v,
                None => {
                  error!("failed to parse multicast tx flow descriptor at offset {descr_offset}");
                  continue;
                }
              };
            if descr.flow_type != 2 {
              error!("wanted to create unknown flow type {}", descr.flow_type);
              continue;
            }
            if descr.flow_id == 0 || descr.flow_id as usize > MAX_TX_FLOWS as _ {
              // MAYBE TODO move this check to tx_multicasts
              error!("wanted to create multicast tx flow with invalid flow id: {}", descr.flow_id);
              continue;
            }
            let flow_index = (descr.flow_id as usize) - 1;
            let channel_indices = channel_ids
              .iter()
              .map(|&id| if id > 0 { Some((id - 1) as usize) } else { None })
              .collect_vec();
            if let Err(e) = validate_flow_layout(
              &channel_indices,
              FPP_MAX_ADVERTISED.into(),
              (self_info.bits_per_sample / 8).into(),
              self_info.tx_channels.len(),
            ) {
              error!("refusing multicast tx flow id {}: {e}", descr.flow_id);
              continue;
            }
            {
              let mut flows_tx_opt = flows_tx.lock().await;
              let flows_tx = if let Some(flows_tx) = flows_tx_opt.as_mut() {
                flows_tx
              } else {
                error!("trying to create multicast tx flow but we have no flows transmitter active");
                continue;
              };
              if flows_tx.get_flows_info()[flow_index].is_some() {
                // TODO move this check to tx_multicasts or flows_tx
                error!("tx flow id busy: {}", descr.flow_id);
                continue;
              }
            }

            if let Some(txm) = tx_multicasts.lock().await.as_ref() {
              if let Err(e) = txm.add_flow(flow_index, channel_indices).await {
                error!("adding multicast tx flow id {} failed: {e:?}", descr.flow_id);
                continue;
              }
            } else {
              error!("tx_multicasts None but got add multicast request");
              continue;
            }
            flow_ids.push(flow_index + 1);
          }
          if flow_ids.len() > 0 {
            let mut response = ByteBuffer::new();
            response.write_u16(flow_ids.len().try_into().unwrap());
            response.write_u16(0);
            for id in flow_ids {
              response.write_u16(id.try_into().unwrap());
            }
            conn.respond(response.as_bytes()).await;
          } else {
            conn.respond_with_code(0xFFFF /* TODO */, &[]).await;
          }
        }
        delete_multicast_tx_flow::OPCODE => {
          let content = request.content();
          let flow_ids = match delete_multicast_tx_flow::parse_flow_ids(content) {
            Some(ids) => ids,
            None => {
              error!("delete multicast tx flow: packet too short: {}", hex::encode(content));
              conn.respond_with_code(0xFFFF /* TODO */, &[]).await;
              continue;
            }
          };
          let flow_indices = flow_ids
            .into_iter()
            .filter_map(|id| if id > 0 { Some((id as usize) - 1) } else { None })
            .filter(|&index| index < MAX_TX_FLOWS as usize);

          let mut deleted_any = false;

          for flow_index in flow_indices {
            if let Some(txm) = tx_multicasts.lock().await.as_ref() {
              txm
                .remove_flow(flow_index)
                .await
                .map(|()| {
                  deleted_any = true;
                  info!("deleted multicast tx flow id {}", flow_index + 1);
                })
                .log_and_forget();
            } else {
              error!("tx_multicasts None but got delete multicast request");
              continue;
            }
          }
          if deleted_any {
            conn.respond(&[]).await;
          } else {
            conn.respond_with_code(0xFFFF /* TODO */, &[]).await;
          }
        }

        0x2320 => {
          // ???
          conn.respond_with_code(0x30, &[]).await;
        }

        0x1001 => {
          // Set device name (content: the name, NUL-terminated; protocol
          // 0x2809) or, with no content, reset it to the factory name.
          // The name lives with the host application (it is in NAME and in
          // the mDNS records, flows and channel instance names), so the
          // request is handed to the host through NAME_REQUEST_PATH: a
          // one-line file holding the new name, empty for a reset. The
          // host renames and restarts the device.
          let code = match &self_info.name_request_path {
            None => 0x30, // unsupported
            Some(path) => match requested_device_name(request.content()) {
              Err(why) => {
                warn!("refusing device rename: {why}");
                0x30
              }
              Ok(name) => match write_name_request(path, &name) {
                Ok(()) => {
                  info!("device rename requested by a controller: {:?}", name.as_deref().unwrap_or("<factory>"));
                  1
                }
                Err(e) => {
                  error!("cannot hand rename request to the host ({}): {e}", path.display());
                  0x30
                }
              },
            },
          };
          conn.respond_with_code(code, &[]).await;
        }

        0x2204 => {
          // TX flow labels, asked for by controllers next to the TX flow
          // query (content 0001 0001 0000: first page). It went unanswered
          // ("received unknown opcode1 0x2204") so the controller kept
          // waiting for it. This device keeps no flow labels: an empty page
          // in the same form as the other paged replies (02 = page size
          // marker, 00 = no records), as netaudio's own virtual device
          // answers it.
          conn.respond_with_code(1, &[0x02, 0x00]).await;
        }

        query_rx_flows::OPCODE => {
          // query RX flows
          let content = request.content();
          let (code, response) = if let Some(chsub) = subscriber.as_ref() {
            paginate_make_response(
              &mut conn,
              content,
              MAX_RX_FLOWS.min(16).try_into().unwrap(),
              chsub.flows_info().read().unwrap().iter().enumerate(),
              |(flow_index, flow_opt), bytes| -> Option<u16> {
                flow_opt.as_ref().map(|flow_info| -> u16 {
                  align_wpos(bytes, 4);
                  let receiver_socket_descriptor_offset = bytes.get_wpos().try_into().unwrap();
                  bytes.write_bytes(
                    DestinationSocketDescriptor {
                      unknown1_8002: 0x8002,
                      port: flow_info.rx_port,
                      addr: self_info.ip_address.octets(),
                    }
                    .binary_serialize_to_array(binary_serde::Endianness::Big)
                    .as_slice(),
                  );

                  let descriptor2_offset = bytes.get_wpos().try_into().unwrap();
                  bytes.write_bytes(
                    query_rx_flows::Descriptor2 {
                      unknown1_9: 9,
                      unknown2_1: 1,
                      unknown3_800: 0x800,
                      unknown4_0: 0,
                      latency_ns: (flow_info.latency_samples as u64 * 1_000_000_000u64
                        / self_info.sample_rate as u64)
                        .try_into()
                        .unwrap(),
                      unknown5_0: 0,
                    }
                    .binary_serialize_to_array(binary_serde::Endianness::Big)
                    .as_slice(),
                  );

                  let req_bits_in_mask =
                    flow_info.channels_map.iter().map(|bv| bv.len()).max().unwrap_or(0);
                  let words_per_bitmask = ((req_bits_in_mask + 15) / 16).max(1);

                  let bitmask_offsets = flow_info
                    .channels_map
                    .iter()
                    .map(|mask| {
                      let mut chi = 0;
                      let pos = bytes.get_wpos();
                      for _ in 0..words_per_bitmask {
                        let mut word: u16 = 0;
                        let mut single_bit = 1;
                        while single_bit != 0 {
                          word |= if mask.get(chi).unwrap_or(false) { single_bit } else { 0 };
                          chi += 1;
                          single_bit <<= 1;
                        }
                        bytes.write_u16(word);
                      }
                      pos
                    })
                    .collect_vec();

                  align_wpos(bytes, 4);
                  let main_descriptor_offset = bytes.get_wpos();
                  bytes.write_bytes(
                    query_rx_flows::FlowDescriptorHeader {
                      flow_id: (flow_index + 1).try_into().unwrap(),
                      unknown1_1: 1,
                      sample_rate: self_info.sample_rate,
                      unknown2_0: 0,
                      bits_per_sample: self_info.bits_per_sample.into(),
                      unknown3_1: 1,
                      channels_count: flow_info.channels_map.len().try_into().unwrap(),
                      words_per_bitmask: words_per_bitmask.try_into().unwrap(),
                      receiver_socket_descriptor_offset,
                    }
                    .binary_serialize_to_array(binary_serde::Endianness::Big)
                    .as_slice(),
                  );
                  for pos in bitmask_offsets {
                    bytes.write_u16(pos.try_into().unwrap());
                  }
                  bytes.write_bytes(
                    query_rx_flows::FlowDescriptorFooter { descriptor2_offset }
                      .binary_serialize_to_array(binary_serde::Endianness::Big)
                      .as_slice(),
                  );

                  main_descriptor_offset.try_into().unwrap()
                })
              },
            )
          } else {
            (1, vec![])
          };
          conn.respond_with_code(code, &response).await;
        }

        0x1100 => {
          // Device settings, asked for by controllers for the device view
          // (request content: a page header and the property ids wanted).
          // It used to answer 110 zero bytes, so controllers showed no
          // sample rate and no latency. Answer the properties this device
          // has, values from its settings (see device_settings_response).
          conn.respond_with_code(1, &device_settings_response(&self_info)).await;
        }
        0x1101 => {
          // Device settings write: a controller sets the receive latency and/or
          // the sample rate. The host owns these settings, so requests are
          // handed over through the *_REQUEST_PATH files. Reads report the new
          // value at once, so a controller's verify-after-write sees it.
          let content = request.content();
          let lat = requested_latency_ns(content);
          let sr = requested_sample_rate(content);

          let mut code = 1u16;
          let mut wrote_latency = false;
          let mut wrote_sr = false;

          if let Some(ns) = lat {
            match &self_info.latency_request_path {
              None => { code = 0x30; warn!("latency write unsupported (no LATENCY_REQUEST_PATH)"); }
              Some(path) if !(MIN_LATENCY_NS..=MAX_LATENCY_NS).contains(&ns) => {
                code = 0x30; warn!("refusing receive latency {ns} ns");
              }
              Some(path) => match write_latency_request(path, ns) {
                Ok(()) => {
                  info!("receive latency {ns} ns requested by a controller");
                  self_info.announced_latency_ns.store(ns, std::sync::atomic::Ordering::Relaxed);
                  wrote_latency = true;
                }
                Err(e) => { code = 0x30; error!("cannot hand latency request: {e}"); }
              },
            }
          }

          if code == 1 && sr.is_some() {
            let hz = sr.unwrap();
            match &self_info.sample_rate_request_path {
              None => { code = 0x30; warn!("sample-rate write unsupported (no SAMPLE_RATE_REQUEST_PATH)"); }
              Some(path) => match write_sample_rate_request(path, hz) {
                Ok(()) => {
                  info!("sample rate {hz} Hz requested by a controller");
                  self_info.announced_sample_rate.store(hz, std::sync::atomic::Ordering::Relaxed);
                  wrote_sr = true;
                }
                Err(e) => { code = 0x30; error!("cannot hand sample-rate request: {e}"); }
              },
            }
          }

          // If neither property was present, treat as malformed.
          if lat.is_none() && sr.is_none() {
            code = 0x30;
            warn!("settings write without latency or sample rate: {}", hex::encode(content));
          }

          if code == 1 {
            conn.respond_with_code(1, &device_settings_response(&self_info)).await;
            if wrote_latency {
              mcast.send(make_latency_change_notification()).await.log_and_forget();
            }
            if wrote_sr {
              // no dedicated notification; device_settings_response already reflects it
            }
          } else {
            conn.respond_with_code(code, &[]).await;
          }
        }
        0x1102 => {
          // Property directory: which settings exist and how they may be
          // used (was 94 zero bytes).
          conn.respond_with_code(1, &property_directory_response()).await;
        }
        0x3300 => {
          // WTF: this is necessary to avoid 'clock domain mismatch' error in DC
          conn.respond(&[0x38, 0x00, 0x38, 0xfd, 0x38, 0xfe, 0x38, 0xff]).await;
          //conn.respond(&[0u8; 8]).await;
        }

        set_channels_subscriptions::OPCODE => {
          // subscribe (connect our receiver to remote transmitter)
          // or unsubscribe if tx_*_offset is 0
          if let Some(channels_recv) = &subscriber {
            let c_whole = request.content();
            for req in
              deserialize_items::<set_channels_subscriptions::SingleChannelSubscriptionRequest>(c_whole)
            {
              if req.local_channel_id == 0 {
                continue;
              }
              let local_channel_index = (req.local_channel_id - 1).try_into().unwrap();
              if local_channel_index >= self_info.rx_channels.len() {
                error!("got connect/disconnect request for nonexisting channel {}", req.local_channel_id);
                continue;
              }
              if req.tx_channel_name_offset > 0 && req.tx_hostname_offset > 0 {
                let str_or_none = |offset: u16| match offset {
                  _ if offset < (HEADER_LENGTH as _) => None,
                  v => match read_0term_str_from_buffer(&c_whole, v as usize - HEADER_LENGTH) {
                    Ok(s) => Some(s),
                    Err(e) => {
                      error!("failed to decode string: {e:?}");
                      None
                    }
                  },
                };
                let tx_channel_name = str_or_none(req.tx_channel_name_offset);
                let tx_hostname = str_or_none(req.tx_hostname_offset);
                info!(
                  "connection requested: {} <- {:?} @ {:?}",
                  req.local_channel_id, tx_channel_name, tx_hostname
                );
                if tx_channel_name.is_some() && tx_hostname.is_some() {
                  channels_recv
                    .subscribe(local_channel_index, tx_channel_name.unwrap(), tx_hostname.unwrap())
                    .await;
                } else {
                  error!("couldn't read tx names from subscription request: {}", hex::encode(&c_whole));
                }
              } else {
                info!("disconnect requested: local channel {}", req.local_channel_id);
                channels_recv.unsubscribe(local_channel_index).await;
              }
            }
            conn.respond(&[]).await;
          }
        }

        remove_rx_subscriptions::OPCODE => {
          // netaudio subscription remove (used by network-audio-controller)
          // received unknown opcode1 0x3014, content 000100000002
          // whole packet: "27ff00104a1c30140000000100000002"
          if let Some(channels_recv) = &subscriber {
            let content = request.content();
            let ids = match remove_rx_subscriptions::parse_channel_ids(content) {
              Some(ids) if !ids.is_empty() => ids,
              _ => {
                error!("0x3014: no channel in request: {}", hex::encode(content));
                conn.respond_with_code(0xFFFF /* TODO */, &[]).await;
                continue;
              }
            };
            // A bulk remove names every channel; only the first used to be
            // unsubscribed (U1).
            let (indices, invalid) =
              remove_rx_subscriptions::local_channel_indices(&ids, self_info.rx_channels.len());
            for id in &invalid {
              error!("0x3014: disconnect requested for nonexisting channel {id}");
            }
            if indices.is_empty() {
              conn.respond_with_code(0xFFFF /* TODO */, &[]).await;
              continue;
            }
            for local_channel_index in indices {
              info!("disconnect requested: local channel {}", local_channel_index + 1);
              channels_recv.unsubscribe(local_channel_index).await;
            }
            conn.respond(&[]).await;
          }
        }

        0x0086 => {
          // IDENTIFY_QUERY: no-op acknowledgement (handoff W8).
          conn.respond(&[]).await;
        }

        0x00c0 => {
          // SYS_RESET / factory reset: polite refusal (handoff W7).
          warn!("SYS_RESET requested; refusing (not implemented)");
          conn.respond_with_code(0x30, &[]).await;
        }

        0x0090 | 0x00a0 => {
          // Upgrade control/status: refuse with reboot-required class (handoff W7).
          warn!("upgrade requested; refusing (reboot required before starting upgrade)");
          conn.respond_with_code(0x30, &[]).await;
        }

        x => {
          // Conmon/ARC message ids the firmware exposes but Inferno does not
          // implement: refuse with the unsupported-opcode family code instead
          // of staying silent, so controllers stop retrying (handoff W3).
          warn!("received unsupported ARC opcode {x:#04x}, content {}", hex::encode(request.content()));
          conn.respond_with_code(0x30, &[]).await;
        }
      }
    } else {
      warn!(
        "received unsupported ARC opcode2 {:#04x}, content {}",
        request.opcode2().read(),
        hex::encode(request.content())
      );
      conn.respond_with_code(0x30, &[]).await;
    }
  }
}

/// Latency limits announced to controllers: the receive latency can be set
/// within them (0x1101, handed to the host through LATENCY_REQUEST_PATH).
const MIN_LATENCY_NS: u32 = 500_000;
const MAX_LATENCY_NS: u32 = 10_000_000;
const DEFAULT_LATENCY_NS: u32 = 10_000_000;

/// The latency a 0x1101 settings write asks for: the value of its
/// configured (0x8205) or active (0x8301) latency record. Records are
/// (property id, value offset from the start of the packet), behind a
/// one-byte kind and a one-byte count, like the 0x1100 reply.
fn requested_latency_ns(content: &[u8]) -> Option<u32> {
  const HEADER: usize = 10;
  let count = *content.get(1)? as usize;
  for rec in content.get(2..2 + count * 4)?.chunks(4) {
    let id = u16::from_be_bytes([rec[0], rec[1]]);
    if id != 0x8205 && id != 0x8301 {
      continue;
    }
    let off = (u16::from_be_bytes([rec[2], rec[3]]) as usize).checked_sub(HEADER)?;
    let v = content.get(off..off + 4)?;
    return Some(u32::from_be_bytes(v.try_into().ok()?));
  }
  None
}

/// The sample rate a 0x1101 settings write asks for: the value of its
/// sample-rate record (0x8020). Same record layout as the latency one.
fn requested_sample_rate(content: &[u8]) -> Option<u32> {
  const HEADER: usize = 10;
  let count = *content.get(1)? as usize;
  for rec in content.get(2..2 + count * 4)?.chunks(4) {
    let id = u16::from_be_bytes([rec[0], rec[1]]);
    if id != 0x8020 {
      continue;
    }
    let off = (u16::from_be_bytes([rec[2], rec[3]]) as usize).checked_sub(HEADER)?;
    let v = content.get(off..off + 4)?;
    return Some(u32::from_be_bytes(v.try_into().ok()?));
  }
  None
}

/// Writes a sample-rate request for the host atomically (temp file + rename).
fn write_sample_rate_request(path: &std::path::Path, hz: u32) -> std::io::Result<()> {
  let tmp = path.with_extension("tmp");
  std::fs::write(&tmp, format!("{hz}\n"))?;
  std::fs::rename(&tmp, path)
}

/// Writes a latency request for the host atomically (temp file + rename).
fn write_latency_request(path: &std::path::Path, ns: u32) -> std::io::Result<()> {
  let tmp = path.with_extension("tmp");
  std::fs::write(&tmp, format!("{ns}\n"))?;
  std::fs::rename(&tmp, path)
}

/// The notification controllers wait for after a latency change.
fn make_latency_change_notification() -> crate::protocol::mcast::MulticastMessage {
  crate::protocol::mcast::MulticastMessage {
    start_code: 0xffff,
    opcode: [0x07, 0x2a, 0x01, 0x06, 0, 0, 0, 0],
    content: vec![0, 0],
  }
}

/// The 0x1100 device settings reply: a record per property (id, offset of
/// its value from the start of the packet), then the u32 values:
/// sample rate (0x8020), and default/configured/active/maximum/minimum
/// latency in ns (0x8204/0x8205/0x8301/0x8302/0x8306).
fn announced_sample_rate(self_info: &DeviceInfo) -> u32 {
  let a = self_info.announced_sample_rate.load(std::sync::atomic::Ordering::Relaxed);
  if a != 0 { a } else { self_info.sample_rate }
}

fn device_settings_response(self_info: &DeviceInfo) -> Vec<u8> {
  let latency: u32 = self_info.announced_latency_ns.load(std::sync::atomic::Ordering::Relaxed);
  let settings: [(u16, u32); 6] = [
    (0x8020, announced_sample_rate(self_info)),
    (0x8204, DEFAULT_LATENCY_NS),
    (0x8205, latency),
    (0x8301, latency),
    (0x8302, MAX_LATENCY_NS),
    (0x8306, MIN_LATENCY_NS),
  ];
  const HEADER: usize = 10; // the ARC header the reply goes out behind
  let first_value = HEADER + 2 + settings.len() * 4;
  let mut out = vec![0x02, settings.len() as u8];
  for (i, (id, _)) in settings.iter().enumerate() {
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&((first_value + i * 4) as u16).to_be_bytes());
  }
  for (_, value) in settings {
    out.extend_from_slice(&value.to_be_bytes());
  }
  out
}

/// The 0x1102 property directory: a u16 count, then (property id, flags)
/// pairs; flags 1 = read, 3 = read/write. The set a hardware interface
/// announces, as netaudio's virtual device answers it.
fn property_directory_response() -> Vec<u8> {
  const PROPERTIES: [(u16, u16); 31] = [
    (0x8020, 3), (0x8021, 3), (0x0022, 3), (0x0023, 3), (0x0024, 1), (0x8060, 3), (0x0062, 3),
    (0x0063, 1), (0x0201, 3), (0x8204, 3), (0x8205, 3), (0x020a, 1), (0x020b, 1), (0x0210, 3),
    (0x0211, 3), (0x0212, 3), (0x0213, 1), (0x0214, 1), (0x0222, 3), (0x8301, 3), (0x8306, 1),
    (0x8302, 1), (0x8321, 1), (0x0310, 1), (0x0311, 1), (0x0312, 1), (0x0303, 3), (0x83f0, 1),
    (0x0601, 1), (0x0309, 1), (0x0209, 1),
  ];
  let mut out = (PROPERTIES.len() as u16).to_be_bytes().to_vec();
  for (id, flags) in PROPERTIES {
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
  }
  out
}

#[cfg(test)]
mod device_settings_tests {
  use super::*;

  #[test]
  fn device_settings_carry_rate_and_latency() {
    let mut info = crate::device_server::settings::Settings::new("t", "t", Some(std::net::Ipv4Addr::LOCALHOST), &Default::default()).self_info;
    info.sample_rate = 48000;
    info.latency_ns = 10_000_000;
    info.announced_latency_ns.store(10_000_000, std::sync::atomic::Ordering::Relaxed);
    let body = device_settings_response(&info);
    assert_eq!(&body[..2], &[0x02, 6]);
    // Each record's value offset counts the 10-byte ARC header.
    let value = |id: u16| {
      let rec = body[2..2 + 6 * 4].chunks(4).find(|r| u16::from_be_bytes([r[0], r[1]]) == id).unwrap();
      let off = u16::from_be_bytes([rec[2], rec[3]]) as usize - 10;
      u32::from_be_bytes(body[off..off + 4].try_into().unwrap())
    };
    assert_eq!(value(0x8020), 48000);
    assert_eq!(value(0x8301), 10_000_000);
    assert_eq!(value(0x8205), 10_000_000);
    assert_eq!(value(0x8306), MIN_LATENCY_NS);
    assert_eq!(value(0x8302), MAX_LATENCY_NS);
  }

  #[test]
  fn latency_write_requests() {
    // As netaudio sends it: 2 ms in the configured and active records.
    let content = hex::decode("05048205002002110004830100240310000483028306001e8480001e8480").unwrap();
    assert_eq!(requested_latency_ns(&content), Some(2_000_000));
    assert_eq!(requested_latency_ns(&[0x05, 0x01, 0x80, 0x20, 0x00, 0x10, 0, 0, 0, 0]), None);
    assert_eq!(requested_latency_ns(&[0x05, 0x04, 0x82]), None);
    let dir = std::env::temp_dir().join(format!("latreq-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("req");
    write_latency_request(&path, 500_000).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "500000\n");
    std::fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn property_directory_lists_every_property() {
    let body = property_directory_response();
    assert_eq!(u16::from_be_bytes([body[0], body[1]]), 31);
    assert_eq!(body.len(), 2 + 31 * 4);
  }
}

/// The name in a set-device-name request: Some(name), or None for a reset
/// (no content). Names follow the controllers' rules: 1-31 characters,
/// letters, digits and '-', starting with a letter, not ending in '-'.
fn requested_device_name(content: &[u8]) -> Result<Option<String>, String> {
  let raw = match content.iter().position(|&b| b == 0) {
    Some(end) => &content[..end],
    None => content,
  };
  if raw.is_empty() {
    return Ok(None);
  }
  let name = std::str::from_utf8(raw).map_err(|_| "name is not UTF-8".to_owned())?;
  let ok_chars = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
  let starts = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic());
  if name.len() > 31 || !ok_chars || !starts || name.ends_with('-') {
    return Err(format!("invalid device name {name:?}"));
  }
  Ok(Some(name.to_owned()))
}

/// Writes the request for the host atomically (temp file + rename), so it
/// never reads half a name.
fn write_name_request(path: &std::path::Path, name: &Option<String>) -> std::io::Result<()> {
  let tmp = path.with_extension("tmp");
  std::fs::write(&tmp, format!("{}\n", name.as_deref().unwrap_or("")))?;
  std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod rename_tests {
  use super::*;

  #[test]
  fn device_name_requests() {
    assert_eq!(requested_device_name(b"Stage-Left\0"), Ok(Some("Stage-Left".to_owned())));
    assert_eq!(requested_device_name(b""), Ok(None), "no content is a reset");
    assert_eq!(requested_device_name(b"\0"), Ok(None));
    for bad in [&b"9lives\0"[..], b"has space\0", b"trailing-\0", b"under_score\0", &[b'a'; 32][..], b"\xff\xfe\0"] {
      assert!(requested_device_name(bad).is_err(), "{bad:?}");
    }
  }

  #[test]
  fn name_request_file_is_one_line() {
    let dir = std::env::temp_dir().join(format!("inferno-rename-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("name-request");
    write_name_request(&path, &Some("Desk-A".to_owned())).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Desk-A\n");
    write_name_request(&path, &None).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "\n");
    std::fs::remove_dir_all(&dir).unwrap();
  }
}


#[cfg(test)]
mod requested_latency_edge_tests {
  use super::requested_latency_ns;

  #[test]
  fn empty_content_returns_none() {
    assert_eq!(requested_latency_ns(&[]), None);
  }

  #[test]
  fn count_zero_returns_none() {
    assert_eq!(requested_latency_ns(&[0x05, 0x00]), None);
  }

  #[test]
  fn truncated_record_returns_none() {
    // count says 1 but only 2 bytes of record
    assert_eq!(requested_latency_ns(&[0x05, 0x01, 0x82]), None);
  }

  #[test]
  fn offset_before_header_returns_none() {
    // record points at offset 5 (< HEADER 10)
    let content = [0x05, 0x01, 0x82, 0x05, 0x00, 0x05];
    assert_eq!(requested_latency_ns(&content), None);
  }

  #[test]
  fn short_value_returns_none() {
    // record points 10 bytes past the header, but the packet is too short
    // to hold the 4-byte value.
    let content = [0x05, 0x01, 0x82, 0x05, 0x00, 0x14];
    assert_eq!(requested_latency_ns(&content), None);
  }

  #[test]
  fn wrong_property_id_returns_none() {
    let content = [0x05, 0x01, 0x80, 0x20, 0x00, 0x0a, 0, 0, 0, 0, 1, 2, 3, 4];
    assert_eq!(requested_latency_ns(&content), None);
  }
}

#[cfg(test)]
mod sample_rate_write_edge_tests {
  use super::requested_sample_rate;

  #[test]
  fn empty_returns_none() {
    assert_eq!(requested_sample_rate(&[]), None);
  }

  #[test]
  fn count_zero_returns_none() {
    assert_eq!(requested_sample_rate(&[0x05, 0x00]), None);
  }

  #[test]
  fn parses_sample_rate_record() {
    // 0x8020 record at offset 10, value 44100
    let mut content = vec![0x05, 0x01, 0x80, 0x20, 0x00, 0x0a];
    content.extend_from_slice(&0u8.to_be_bytes()); // pad to offset 10
    content.extend_from_slice(&[0,0,0,0]); // positions 6..10
    // Actually need value at offset 10: content length must be 14
    // Rebuild cleanly:
    let mut c = vec![0u8; 14];
    c[0] = 0x05; c[1] = 0x01;
    c[2] = 0x80; c[3] = 0x20;
    c[4] = 0x00; c[5] = 0x14; // offset 20 -> content offset 10
    c[10..14].copy_from_slice(&44100u32.to_be_bytes());
    assert_eq!(requested_sample_rate(&c), Some(44100));
  }

  #[test]
  fn wrong_id_returns_none() {
    let mut c = vec![0u8; 14];
    c[0] = 0x05; c[1] = 0x01;
    c[2] = 0x82; c[3] = 0x05; // latency id
    c[4] = 0x00; c[5] = 0x14;
    c[10..14].copy_from_slice(&2_000_000u32.to_be_bytes());
    assert_eq!(requested_sample_rate(&c), None);
  }
}

#[cfg(test)]
mod device_name_edge_tests {
  use super::requested_device_name;

  #[test]
  fn exactly_31_chars_accepted() {
    let name = "a".repeat(31);
    let mut buf = name.as_bytes().to_vec();
    buf.push(0);
    assert_eq!(requested_device_name(&buf), Ok(Some(name)));
  }

  #[test]
  fn no_terminator_full_31_accepted() {
    let name = "a".repeat(31);
    assert_eq!(requested_device_name(name.as_bytes()), Ok(Some(name)));
  }
}

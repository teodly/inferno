use crate::net_utils::{create_mio_udp_socket, MAX_PAYLOAD_BYTES};
use crate::samples_collector::SamplesCollector;
use crate::state_storage::StateStorage;
use crate::MediaClock;
use crate::{
  common::*, mdns_client::AdvertisedChannel,
  protocol::flows_control::FlowHandle,
};

use crate::device_info::DeviceInfo;
use crate::{
  flows_rx::FlowsReceiver, info_mcast_server::MulticastMessage, mdns_client::MdnsClient,
  protocol::flows_control::FlowsControlClient,
};

use crate::ring_buffer::{self, ExternalBuffer, ExternalBufferParameters, OwnedBuffer, ProxyToSamplesBuffer, RBInput, RBOutput};

use atomic::Atomic;
use futures::{future::join_all, Future, FutureExt};
use itertools::Itertools;
use std::collections::btree_map::Entry;
use std::{
  collections::{BTreeMap, BTreeSet},
  net::SocketAddr,
  pin::Pin,
  sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex, RwLock,
  },
  time::{Duration, Instant},
};
use tokio::{sync::mpsc, time::sleep, time::timeout};
use serde::{Serialize, Deserialize};

const REORDER_WAIT_SAMPLES: usize = 4800;

enum Command {
  Shutdown,
  Subscribe { local_channel_index: usize, tx_channel_name: String, tx_hostname: String },
  Unsubscribe { local_channel_index: usize },
  ForceRefresh,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum SubscriptionStatus {
  Unresolved = 1,
  InProgress = 8,
  TooManyTxFlows = 0x14, // TODO propagate error from tx's reply
  TxFail = 0x15,
  ReceivingUnicast = 0x01010009,
  ReceivingMulticast = 0x0101000a,
}

impl SubscriptionStatus {
  pub fn is_receiving(&self) -> bool {
    *self == Self::ReceivingUnicast || *self == Self::ReceivingMulticast
  }
}

#[derive(Clone, Debug)]
pub struct SubscriptionInfo {
  pub tx_channel_name: String,
  pub tx_hostname: String,
  pub status: SubscriptionStatus,
}

#[derive(Debug)]
pub struct ChannelsSubscriber {
  commands_sender: mpsc::Sender<Command>,
  subscriptions_info: Arc<RwLock<Vec<Option<SubscriptionInfo>>>>,
}

#[derive(Serialize, Deserialize)]
struct SavedChannelState {
  local_channel_id: u16,
  local_channel_name: String,
  tx_channel_name: String,
  tx_hostname: String
}

#[derive(Serialize, Deserialize)]
struct SavedChannelsState {
  channels: Vec<SavedChannelState>
}

impl ChannelsSubscriber {
  pub fn new<P: ProxyToSamplesBuffer + Send + Sync + 'static, B: ChannelsBuffering<P> + Send + Sync + 'static>(
    self_info: Arc<DeviceInfo>,
    media_clock: Arc<RwLock<MediaClock>>,
    flows_recv: Arc<FlowsReceiver<P>>,
    mdns_client: Arc<MdnsClient>,
    mcast: mpsc::Sender<MulticastMessage>,
    channels_buffering: B,
    state_storage: Arc<StateStorage>,
    ref_instant: Instant,
  ) -> (Self, Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
    let (tx, rx) = mpsc::channel(100);
    let r = Self {
      commands_sender: tx,
      subscriptions_info: Arc::new(RwLock::new(vec![None; self_info.rx_channels.len()])),
    };
    let mut internal = ChannelsSubscriberInternal::<P, B>::new(
      rx,
      self_info,
      media_clock,
      flows_recv,
      mdns_client,
      r.subscriptions_info.clone(),
      mcast,
      channels_buffering,
      state_storage,
      ref_instant,
    );
    return (r, async move { internal.run().await }.boxed());
  }
  pub async fn subscribe(
    &self,
    local_channel_index: usize,
    tx_channel_name: &str,
    tx_hostname: &str,
  ) {
    // we must (TODO: really?) update subscriptions_info here because Dante Controller requests rx subscriptions list right after receiving a response to subscribe request
    self.subscriptions_info.write().unwrap()[local_channel_index] = Some(SubscriptionInfo {
      tx_hostname: tx_hostname.to_owned(),
      tx_channel_name: tx_channel_name.to_owned(),
      status: SubscriptionStatus::Unresolved,
    });
    self
      .commands_sender
      .send(Command::Subscribe {
        local_channel_index,
        tx_channel_name: tx_channel_name.to_owned(),
        tx_hostname: tx_hostname.to_owned(),
      })
      .await
      .log_and_forget();
  }
  pub async fn unsubscribe(&self, local_channel_index: usize) {
    self.commands_sender.send(Command::Unsubscribe { local_channel_index }).await.log_and_forget();
  }
  pub async fn shutdown(&self) {
    self.commands_sender.send(Command::Shutdown).await.log_and_forget();
  }
  pub fn channel_status(&self, channel_index: usize) -> Option<SubscriptionInfo> {
    self.subscriptions_info.read().unwrap()[channel_index].clone()
  }
}



pub trait ChannelsBuffering<P: ProxyToSamplesBuffer> {
  fn connect_channel(&self, start_time: Clock, rb_output: &mut Option<RBOutput<Sample, P>>, channel_index: usize, latency_samples: usize) -> Option<RBInput<Sample, P>>;
  fn disconnect_channel(&self, channel_index: usize);
}

pub struct OwnedBuffering {
  buffer_length: usize,
  hole_fix_wait: usize,
  samples_collector: Arc<SamplesCollector<OwnedBuffer<Atomic<Sample>>>>,
}

impl OwnedBuffering {
  pub fn new(buffer_length: usize, hole_fix_wait: usize, samples_collector: Arc<SamplesCollector<OwnedBuffer<Atomic<Sample>>>>) -> Self {
    Self { buffer_length, hole_fix_wait, samples_collector }
  }
}

impl ChannelsBuffering<OwnedBuffer<Atomic<Sample>>> for OwnedBuffering {
  fn connect_channel(&self, start_time: Clock, rb_output: &mut Option<RBOutput<Sample, OwnedBuffer<Atomic<Sample>>>>, channel_index: usize, latency_samples: usize) -> Option<RBInput<Sample, OwnedBuffer<Atomic<Sample>>>> {
    let (sink, source) = if rb_output.is_none() {
      let (sink, source) =
        ring_buffer::new_owned::<Sample>(self.buffer_length, start_time as usize, self.hole_fix_wait);
      *rb_output = Some(source.clone());
      (Some(sink), source)
    } else {
      (None, rb_output.as_ref().unwrap().clone())
    };
    let sc = self.samples_collector.clone();
    tokio::spawn(async move {
      sc.connect_channel(channel_index, source, latency_samples).await;
    });
    sink
  }
  fn disconnect_channel(&self, channel_index: usize) {
    let sc = self.samples_collector.clone();
    tokio::spawn(async move {
      sc.disconnect_channel(channel_index).await;
    });
  }
}

pub struct ExternalBuffering {
  channels: Vec<ExternalBufferParameters<Sample>>,
  hole_fix_wait: usize,
}

impl ExternalBuffering {
  pub fn new(channels: Vec<ExternalBufferParameters<Sample>>, hole_fix_wait: usize) -> Self {
    Self {
      channels,
      hole_fix_wait
    }
  }
}

impl ChannelsBuffering<ExternalBuffer<Atomic<Sample>>> for ExternalBuffering {
  fn connect_channel(&self, start_time: Clock, rb_output: &mut Option<RBOutput<Sample, ExternalBuffer<Atomic<Sample>>>>, channel_index: usize, latency_samples: usize) -> Option<RBInput<Sample, ExternalBuffer<Atomic<Sample>>>> {
    debug_assert!(rb_output.is_none());
    Some(ring_buffer::wrap_external_sink(&self.channels[channel_index], start_time as usize, self.hole_fix_wait))
  }
  fn disconnect_channel(&self, _channel_index: usize) {
  }
}

struct UnicastFlow {
  control_remote_addr: SocketAddr,
  dbcp1: u16,
  handle: Option<FlowHandle>,
}

struct MulticastFlow {
  bundle_full_name: String,
}

enum FlowSource {
  Unicast(UnicastFlow),
  Multicast(MulticastFlow)
}

struct Flow<P: ProxyToSamplesBuffer> {
  local_id: usize,
  source: FlowSource,
  channels_refcount: Vec<isize>,
  channels_rb_outputs: Vec<Option<RBOutput<Sample, P>>>,
  tx_channels: Vec<Option<u16>>,
  latency_samples: usize,
  last_packet_time: Arc<AtomicUsize>,
  needs_subscription_info_update: bool,
  creation_time: usize,
}

impl<P: ProxyToSamplesBuffer> Flow<P> {
  fn new(
    local_id: usize,
    source: FlowSource,
    total_channels: usize,
    tx_channels: impl IntoIterator<Item = u16>,
    latency_samples: usize,
    now: usize,
  ) -> Self {
    let chs = vec![0; total_channels];
    let mut tx_channels = tx_channels.into_iter().map(|ch| Some(ch)).collect_vec();
    tx_channels.resize(total_channels, None);
    Self {
      local_id,
      source,
      channels_refcount: chs,
      channels_rb_outputs: (0..total_channels).map(|_| None).collect(),
      tx_channels,
      latency_samples,
      last_packet_time: Arc::new(AtomicUsize::new(0)),
      needs_subscription_info_update: true,
      creation_time: now,
    }
  }
  fn is_used(&self) -> bool {
    return self.channels_refcount.iter().find(|rc| **rc > 0).is_some();
  }
}

#[derive(Clone, Debug)]
struct ChannelOtherEnd {
  local_flow_index: usize,
  channel_in_flow: usize,
  tx_channel_id: u16,
}

#[derive(Clone, Debug)]
struct ChannelSubscription {
  tx_channel_name: String,
  tx_hostname: String,
  remote: Option<ChannelOtherEnd>,
}

struct ChannelsSubscriberInternal<P: ProxyToSamplesBuffer, B: ChannelsBuffering<P>> {
  commands_receiver: mpsc::Receiver<Command>,
  self_info: Arc<DeviceInfo>,
  media_clock: Arc<RwLock<MediaClock>>,
  channels: Vec<Option<ChannelSubscription>>,
  flows: Arc<Mutex<Vec<Option<Flow<P>>>>>,
  flows_recv: Arc<FlowsReceiver<P>>,
  //buffered_samples_per_channel: usize,
  min_latency_ns: usize,
  control_client: Arc<FlowsControlClient>,
  mdns_client: Arc<MdnsClient>,
  subscriptions_info: Arc<RwLock<Vec<Option<SubscriptionInfo>>>>,
  mcast: mpsc::Sender<MulticastMessage>,
  channels_buffering: B,
  //samples_collector: Arc<SamplesCollector<P>>,
  state_storage: Arc<StateStorage>,
  ref_instant: Instant,
  needs_resolving: bool,
  resolve_now: bool,
}

impl<P: ProxyToSamplesBuffer + Sync + Send + 'static, B: ChannelsBuffering<P>> ChannelsSubscriberInternal<P, B> {
  pub fn new(
    commands_receiver: mpsc::Receiver<Command>,
    self_info: Arc<DeviceInfo>,
    media_clock: Arc<RwLock<MediaClock>>,
    flows_recv: Arc<FlowsReceiver<P>>,
    mdns_client: Arc<MdnsClient>,
    subscriptions_info: Arc<RwLock<Vec<Option<SubscriptionInfo>>>>,
    mcast: mpsc::Sender<MulticastMessage>,
    channels_buffering: B,
    state_storage: Arc<StateStorage>,
    ref_instant: Instant,
  ) -> Self {
    let num_channels = self_info.rx_channels.len();
    return Self {
      commands_receiver,
      self_info: self_info.clone(),
      media_clock,
      channels: vec![None; num_channels],
      flows: Arc::new(Mutex::new(vec![])),
      flows_recv,
      //buffered_samples_per_channel: 524288,
      min_latency_ns: 10_000_000, // TODO dehardcode
      control_client: Arc::new(FlowsControlClient::new(self_info)),
      mdns_client,
      subscriptions_info,
      mcast,
      channels_buffering,
      state_storage,
      ref_instant,
      needs_resolving: false,
      resolve_now: false,
    };
  }
  async fn notify_channels_change(&self, channel_indices: impl IntoIterator<Item = usize>) {
    let mut mask: u8 = 0;
    for ch in channel_indices {
      if ch >= 8 {
        warn!("FIXME: unable to multicast change to rx channel index={ch} properly, >= 8");
        mask |= 1;
        continue;
      }
      mask |= 1 << ch;
    }
    self
      .mcast
      .send(MulticastMessage {
        start_code: 0xffff,
        opcode: [0x07, 0x2a, 1, 2, 0, 0, 0, 0],
        content: [0u8, 1, mask].to_vec(),
      })
      .await
      .log_and_forget();
  }
  pub async fn unsubscribe(&mut self, local_channel_index: usize, remove_from_info: bool) {
    self.channels_buffering.disconnect_channel(local_channel_index);
    if let Some(subscription) = &self.channels[local_channel_index] {
      if let Some(remote) = subscription.remote.as_ref() {
        match &mut self.flows.lock().unwrap()[remote.local_flow_index] {
          Some(flow) => {
            flow.channels_refcount[remote.channel_in_flow] -= 1;
          }
          None => {
            error!("BUG: trying to unsubscribe channel from not existing flow");
          }
        }
      }
      self.channels[local_channel_index] = None;
      if remove_from_info {
        self.subscriptions_info.write().unwrap()[local_channel_index] = None;
      }
      self.notify_channels_change([local_channel_index]).await;
    }
  }
  pub async fn subscribe(
    &mut self,
    local_channel_index: usize,
    tx_channel_name: &str,
    tx_hostname: &str,
  ) {
    self.unsubscribe(local_channel_index, false).await;
    self.channels[local_channel_index] = Some(ChannelSubscription {
      tx_channel_name: tx_channel_name.to_owned(),
      tx_hostname: tx_hostname.to_owned(),
      remote: None,
    });
    self.needs_resolving = true;
    self.resolve_now = true;
  }

  pub async fn resolve_subscriptions(&mut self) -> bool {
    self.resolve_now = false;
    let self_ip = self.self_info.ip_address;

    #[derive(Debug)]
    struct ChannelSourceUpdate {
      local_channel_indices: Vec<usize>,
      remote: ChannelOtherEnd,
    }

    let mut resolved = Vec::<ChannelSourceUpdate>::new();

    // Find channels we need to subscribe:
    let mut needed_channel_indices: BTreeSet<_> = self
      .channels
      .iter()
      .enumerate()
      .filter(|(_, chsub)| match chsub {
        None => false,
        Some(sub) => sub.remote.is_none(),
      })
      .map(|(i, _)| i)
      .collect();

    if needed_channel_indices.is_empty() {
      debug!("no unresolved channels - suspending subscriptions resolver");
      self.needs_resolving = false;
    }

    debug!("we need to subscribe (local channel indices): {needed_channel_indices:?}");

    // Find channels connected to the same tx channels (aliases):
    let mut channel_index_aliases = BTreeMap::<usize, Vec<usize>>::new();
    {
      let mut alias_map = BTreeMap::<(String, String), usize>::new();
      needed_channel_indices.retain(|&ch_index| {
        let ch = self.channels[ch_index].as_ref().unwrap();
        match alias_map.entry((ch.tx_hostname.clone(), ch.tx_channel_name.clone())) {
          Entry::Occupied(existing_index) => {
            channel_index_aliases.entry(*existing_index.get()).or_default().push(ch_index);
            return false;
          }
          Entry::Vacant(entry) => {
            assert!(!channel_index_aliases.contains_key(&ch_index));
            channel_index_aliases.entry(ch_index).or_default().push(ch_index);
            // yes, channel is also its own alias (it simplifies the rest of code)
            entry.insert(ch_index);
            return true;
          }
        }
      });
    }

    debug!("after dealiasing (local channel indices): {needed_channel_indices:?}");

    // Find channels we already have successfully subscribed and use them (no requests needed):
    needed_channel_indices.retain(|&ch_index| {
      let needed_channel = self.channels[ch_index].as_ref().unwrap();
      let remote_opt = self
        .channels
        .iter()
        .enumerate()
        .filter_map(|(_, chsub)| chsub.as_ref())
        .find(|chsub| {
          chsub.remote.is_some()
            && chsub.tx_channel_name == needed_channel.tx_channel_name
            && chsub.tx_hostname == needed_channel.tx_hostname
        })
        .map(|chsub| chsub.remote.as_ref().unwrap());
      if let Some(remote) = remote_opt {
        debug!("channel index={ch_index} already subscribed to {remote:?}, reusing");
        resolved.push(ChannelSourceUpdate {
          local_channel_indices: channel_index_aliases[&ch_index].clone(),
          remote: remote.clone(),
        });
        return false;
      }
      return true;
    });

    trace!("before mdns");

    // Resolve MDNS _chan services:
    // TODO resolve more channels in single query instead of spawning multiple resolvers
    let dns_resolve_futures =
      needed_channel_indices.iter().enumerate().map(|(i, &local_ch_index)| {
        let chsub = self.channels[local_ch_index].as_ref().unwrap();
        let mdns_client = self.mdns_client.clone();
        async move {
          sleep(Duration::from_millis(8 * i as u64)).await;
          let result = mdns_client.query_chan(&chsub.tx_hostname, &chsub.tx_channel_name).await;
          debug!("{:?}", result);
          return (
            local_ch_index,
            match result {
              Ok(v) => Some(v),
              Err(_) => None,
            },
          );
        }
        .boxed()
      });

    trace!("after mdns");

    // Add all needed servers to map:
    let mut channels_per_server = BTreeMap::<SocketAddr, BTreeMap<usize, AdvertisedChannel>>::new();
    let mut channels_per_bundle = BTreeMap::<String, BTreeMap<usize, AdvertisedChannel>>::new();
    for (lci, advserv_opt) in join_all(dns_resolve_futures).await.into_iter() {
      if let Some(advserv) = advserv_opt {
        match &advserv.multicast {
          None => {
            channels_per_server.entry(advserv.addr).or_default().insert(lci, advserv);
          },
          Some(mcast) => {
            // this channel is readily available in multicast, no need to send any requests
            let bundle_name = format!("{}@{}", mcast.bundle_id, mcast.tx_hostname);
            info!("using multicast for channel {} of {}", advserv.tx_channel_id, mcast.tx_hostname);
            channels_per_bundle.entry(bundle_name).or_default().insert(lci, advserv);
          }
        }
      }
    }

    // Check for surprises:
    let bad_servers = channels_per_server
      .iter()
      .filter(|(_addr, entries)| {
        entries.iter().map(|(_, advch)| advch.tx_channels_per_flow).unique().count() != 1
          || entries.iter().map(|(_, advch)| advch.bits_per_sample).unique().count() != 1
          || entries.iter().map(|(_, advch)| advch.dbcp1).unique().count() != 1
      })
      .map(|(&addr, _)| addr)
      .collect_vec();
    for addr in bad_servers {
      error!("Server {addr} has different values of nchan, bits per sample or dbcp1. This is unsupported.");
      channels_per_server.remove(&addr);
    }

    // TODO check sample rate so that we don't request flows from devices with incompatible sample rate

    // Resolve multicast bundles:
    let dns_resolve_futures =
      channels_per_bundle.keys().enumerate().map(|(i, bund_full_name)| {
        let mdns_client = self.mdns_client.clone();
        let bund_full_name = bund_full_name.clone();
        async move {
          sleep(Duration::from_millis(8 * i as u64)).await;
          let result = mdns_client.query_bund(&bund_full_name).await;
          debug!("{:?}", result);
          match result {
            Ok(v) => Some((bund_full_name, v)),
            Err(_) => None,
          }
        }
        .boxed()
      });
    let mut bundles: BTreeMap<_, _> = join_all(dns_resolve_futures).await.into_iter().flatten().collect();

    let mut free_slots_per_server = BTreeMap::<SocketAddr, BTreeMap<usize, BTreeSet<usize>>>::new();
    let mut futures_per_server = BTreeMap::<SocketAddr, Vec<_>>::new();
    {
      trace!("before create futures");
      let mut flows_locked = self.flows.lock().unwrap();
      trace!("acquired flows lock");
      for (flow_index, flow_opt) in flows_locked.iter().enumerate() {
        if let Some(flow) = flow_opt {
          match &flow.source {
            FlowSource::Unicast(uf) => {
              match channels_per_server.entry(uf.control_remote_addr) {
                Entry::Vacant(_) => continue,
                Entry::Occupied(mut channels) => {
                  // here we've found list of channels that may need this flow.

                  // Find if we already have needed tx channels in any flows (no requests needed):
                  // (this is needed because user may have disconnected a channel
                  //  but audio is still flowing in a flow serving different channel(s))

                  // let's iterate over all channel we need from this server:
                  let mut updates = channels
                    .get()
                    .iter()
                    .filter_map(|(lci, advch)| {
                      match flow
                        .tx_channels
                        .iter()
                        .enumerate()
                        .position(|(_, &txch)| txch == Some(advch.tx_channel_id))
                      {
                        None => None,
                        Some(channel_in_flow) => Some(ChannelSourceUpdate {
                          // here we've found a channel that we need
                          local_channel_indices: channel_index_aliases[lci].clone(),
                          remote: ChannelOtherEnd {
                            local_flow_index: flow_index,
                            channel_in_flow,
                            tx_channel_id: advch.tx_channel_id,
                          },
                        }),
                      }
                    })
                    .collect_vec();

                  // And prepare list of free slots in flows transmitters, we'll need them soon:
                  let free_slots = free_slots_per_server
                    .entry(uf.control_remote_addr)
                    .or_default()
                    .entry(flow_index)
                    .or_default();
                  for (channel_in_flow, &refcount) in flow.channels_refcount.iter().enumerate() {
                    if refcount <= 0 {
                      debug_assert_eq!(refcount, 0);
                      free_slots.insert(channel_in_flow);
                    }
                  }
                  for update in &updates {
                    // remove found channels from list of needed channels:
                    let result = channels.get_mut().remove(&update.local_channel_indices[0]);
                    debug_assert!(result.is_some());
                    // and remove slots we'll use soon from the list of free slots:
                    free_slots.remove(&update.remote.channel_in_flow);
                  }
                  resolved.append(&mut updates);
                }
              }
            },
            FlowSource::Multicast(mf) => {
              match channels_per_bundle.entry(mf.bundle_full_name.clone()) {
                Entry::Vacant(_) => continue,
                Entry::Occupied(channels) => {
                  bundles.remove(&mf.bundle_full_name); // already receiving

                  let mut updates = channels
                    .get()
                    .iter()
                    .map(|(lci, advch)| {
                      ChannelSourceUpdate {
                        local_channel_indices: channel_index_aliases[lci].clone(),
                        remote: ChannelOtherEnd {
                          local_flow_index: flow_index,
                          channel_in_flow: advch.multicast.as_ref().unwrap().channel_in_bundle,
                          tx_channel_id: advch.tx_channel_id,
                        },
                      }
                    }).collect_vec();
                  resolved.append(&mut updates);
                }
              }
            },
          }
        }
      }

      // Use free slots:
      for (&server_addr, channels) in &mut channels_per_server {
        if channels.is_empty() {
          continue;
        }
        match free_slots_per_server.entry(server_addr) {
          Entry::Vacant(_) => continue,
          Entry::Occupied(free_slots) => {
            let mut all_free_slots = vec![];
            for (flow_index, slots_in_flow) in free_slots.get() {
              for ch_in_flow in slots_in_flow {
                all_free_slots.push((*flow_index, *ch_in_flow));
              }
            }
            let mut all_free_slots_iter = all_free_slots.iter();
            let mut tx_ch_updates_per_flow =
              BTreeMap::<usize, (Vec<Option<u16>>, Vec<ChannelSourceUpdate>)>::new();
            channels.retain(|&lci, advch| {
              if let Some(&(flow_index, ch_in_flow)) = all_free_slots_iter.next() {
                debug_assert_eq!(flows_locked[flow_index].as_ref().unwrap().channels_refcount[ch_in_flow], 0);
                let entry = tx_ch_updates_per_flow.entry(flow_index).or_insert_with(|| {
                  (flows_locked[flow_index].as_ref().unwrap().tx_channels.clone(), vec![])
                });
                if let Some(prev_tx_ch) = entry.0[ch_in_flow] {
                  warn!("Reusing channel {ch_in_flow} in flow index={flow_index}: changing tx channel {prev_tx_ch} to {}. This behaviour is different from official Dante devices. Please report bug if it doesn't work for you.", advch.tx_channel_id);
                  debug_assert_ne!(prev_tx_ch, advch.tx_channel_id, "looks like finding already subscribed channels didn't work");
                }
                entry.0[ch_in_flow] = Some(advch.tx_channel_id);
                entry.1.push(ChannelSourceUpdate {
                  local_channel_indices: channel_index_aliases[&lci].clone(),
                  remote: ChannelOtherEnd {
                    local_flow_index: flow_index, channel_in_flow: ch_in_flow, tx_channel_id: advch.tx_channel_id
                  }
                });
                return false;
              }
              return true;
            });
            let futures =
              tx_ch_updates_per_flow.into_iter().map(|(flow_index, (new_tx_channels, updates))| {
                let control_client = self.control_client.clone();
                let flow = flows_locked[flow_index].as_ref().unwrap();
                if let FlowSource::Unicast(uf) = &flow.source {
                  let dbcp1 = uf.dbcp1;
                  let handle = uf.handle.unwrap();
                  async move {
                    match control_client
                      .update_flow(&server_addr, dbcp1, handle, &new_tx_channels)
                      .await
                    {
                      Ok(()) => {
                        debug!("update flow {updates:?} request success");
                        Some(updates)
                      }
                      Err(e) => {
                        error!("update flow {updates:?} request failed: {e:?}");
                        // TODO set subscription info with the fail
                        None
                      }
                    }
                  }
                  .boxed()
                } else {
                  panic!("unexpected non-unicast flow");
                }
              });
            futures_per_server.insert(server_addr, futures.collect_vec());
          }
        }
      }

      // Create flows for multicast receivers:
      // TODO make DC display warning when removing multicast flow from the transmitter
      for (bundle_full_name, advbundle) in &bundles {
        let socket = match mio::net::UdpSocket::bind(advbundle.media_addr) {
          Ok(socket) => socket,
          Err(e) => {
            error!("failed to bind to socket {:?} for multicast media: {e:?}", advbundle.media_addr);
            continue;
          }
        };
        let mcast_ipv4 = match advbundle.media_addr.ip() {
          std::net::IpAddr::V4(ip) => ip,
          _ => panic!("media multicast address is not ipv4")
        };
        if let Err(e) = socket.join_multicast_v4(&mcast_ipv4, &self.self_info.ip_address) {
          error!("failed to join multicast group {:?}: {e:?}", advbundle.media_addr.ip());
          continue;
        };
        let flow_index = match flows_locked.iter().position(|o| o.is_none()) {
          Some(i) => i,
          None => {
            flows_locked.push(None);
            flows_locked.len() - 1
          }
        };
        let flow_id = flow_index + 1;
        let source = FlowSource::Multicast(MulticastFlow {
          bundle_full_name: bundle_full_name.clone(),
        });
        let needed_channels = channels_per_bundle.get(bundle_full_name).unwrap();
        let flow = Flow::new(
          flow_id,
          source,
          advbundle.tx_channels_per_flow,
          vec![], // tx_channels aren't read in case of multicast flows, so no need to initialize
          ((advbundle.min_rx_latency_ns as u64).max(self.min_latency_ns as u64) * (self.self_info.sample_rate as u64) / 1_000_000_000u64).try_into().unwrap(),
          self.ref_instant.elapsed().as_secs() as _,
        );
        let time_arc = flow.last_packet_time.clone();
        flows_locked[flow_index] = Some(flow);
        let flows_recv = self.flows_recv.clone();
        let media_addr = advbundle.media_addr;
        let advbundle = advbundle.clone();
        let updates = Some(needed_channels.iter().map(|(lci, advch)| {
          ChannelSourceUpdate {
            local_channel_indices: channel_index_aliases[lci].clone(),
            remote: ChannelOtherEnd {
              local_flow_index: flow_index,
              channel_in_flow: advch.multicast.as_ref().unwrap().channel_in_bundle,
              tx_channel_id: advch.tx_channel_id,
            },
          }
        }).collect_vec());
        let future = async move {
          flows_recv
            .add_socket(
              flow_index,
              socket,
              false,
              (advbundle.bits_per_sample as usize) / 8,
              advbundle.tx_channels_per_flow,
              time_arc,
            )
            .await;
          updates
        }.boxed();
        futures_per_server.entry(media_addr).or_default().push(future);
      }

      // Create new flows for remaining channels:
      for (server_addr, channels) in &mut channels_per_server {
        if channels.is_empty() {
          continue;
        }
        let first = (*channels.first_entry().unwrap().get()).clone();
        for chunk in &channels.iter().chunks(first.tx_channels_per_flow) {
          let chunk = chunk.collect_vec();
          let flow_index = match flows_locked.iter().position(|o| o.is_none()) {
            Some(i) => i,
            None => {
              flows_locked.push(None);
              flows_locked.len() - 1
            }
          };
          let tx_channels = chunk.iter().map(|(_, chadv)| chadv.tx_channel_id);
          let flow_id = flow_index + 1;
          let flow_name = format!("{}_{}", self.self_info.process_id, flow_id);
          let source = FlowSource::Unicast(UnicastFlow {control_remote_addr: first.addr, dbcp1: first.dbcp1, handle: None});
          let flow = Flow::new(
            flow_id,
            source,
            first.tx_channels_per_flow.min(self.self_info.rx_channels.len()).min(8), /*TODO make it configurable*/
            tx_channels,
            ((first.min_rx_latency_ns as u64).max(self.min_latency_ns as u64) * (self.self_info.sample_rate as u64) / 1_000_000_000u64).try_into().unwrap(),
            self.ref_instant.elapsed().as_secs() as _,
          );

          let time_arc = flow.last_packet_time.clone();
          let tx_channels = flow.tx_channels.clone();
          flows_locked[flow_index] = Some(flow);

          let updates = chunk
            .iter()
            .enumerate()
            .map(|(i, (&lci, chadv))| ChannelSourceUpdate {
              local_channel_indices: channel_index_aliases[&lci].clone(),
              remote: ChannelOtherEnd {
                local_flow_index: flow_index,
                channel_in_flow: i,
                tx_channel_id: chadv.tx_channel_id,
              },
            })
            .collect_vec();

          let flows = self.flows.clone();
          let flows_recv = self.flows_recv.clone();
          let control_client = self.control_client.clone();
          let subscription_info = self.subscriptions_info.clone();
          let sample_rate = self.self_info.sample_rate;
          
          let future = async move {
            let (socket, port) = match create_mio_udp_socket(self_ip) {
              Ok(v) => v,
              Err(e) => {
                error!("flow receiver socket creation failed: {e:?}");
                flows.lock().unwrap()[flow_index] = None;
                return None;
              }
            };
            
            let fpp_mtu_limit = (MAX_PAYLOAD_BYTES / (tx_channels.len() * (first.bits_per_sample as usize)/8)).min(u16::MAX as usize) as u16;
            let handle = match control_client
              .request_flow(
                &first.addr,
                first.dbcp1,
                sample_rate,
                first.bits_per_sample,
                first.fpp_max.min(fpp_mtu_limit), // TODO fpp selection, here we use max for lowest overhead
                &tx_channels,
                port,
                &flow_name,
              )
              .await
            {
              Ok(v) => v,
              Err(e) => {
                error!("failed to request flow from {:?}: {e:?}", first.addr);
                let mut subinfos = subscription_info.write().unwrap();
                for update in updates {
                  for chi in update.local_channel_indices {
                    subinfos[chi].as_mut().unwrap().status = SubscriptionStatus::TxFail;
                  }
                }
                flows.lock().unwrap()[flow_index] = None;
                return None;
              }
            };
            assert_eq!(first.bits_per_sample % 8, 0);
            flows_recv
              .add_socket(
                flow_index,
                socket,
                true,
                (first.bits_per_sample as usize) / 8,
                tx_channels.len(),
                time_arc,
              )
              .await;
            let mut flows_locked = flows.lock().unwrap();
            let mut source = &mut flows_locked[flow_index].as_mut().unwrap().source;
            if let FlowSource::Unicast(uf) = &mut source {
              uf.handle = Some(handle);
            } else {
              error!("BUG: unexpected non-unicast flow when trying to assign handle");
            }
            
            return Some(updates);
          }
          .boxed();
          futures_per_server.entry(*server_addr).or_default().push(future);
        }
      }
      trace!("after create futures");
    }

    let (tx0, mut rx) = mpsc::channel(100);

    for (_addr, futures) in futures_per_server {
      let tx = tx0.clone();
      tokio::spawn(async move {
        trace!("inside futures executor start");
        for future in futures {
          if let Some(updates) = future.await {
            tx.send(updates).await.log_and_forget();
          } // TODO else set self.resolve_now = true to retry
        }
        trace!("inside futures executor done");
      });
    }
    tx0.send(resolved).await.log_and_forget();
    drop(tx0);
    while let Some(updates) = rx.recv().await {
      trace!("recv something");
      let mut changed_channels = vec![];
      let now = self.media_clock.read().unwrap().now_in_timebase(self.self_info.sample_rate as u64);
      let now = match now {
        Some(v) => v,
        None => {
          error!("can't subscribe, clock not ready");
          continue;
        }
      };
      for upd in updates {
        let first_index = upd.local_channel_indices[0];
        info!(
          "subscription successful, waiting for media: channel {} and {} aliases",
          self.self_info.rx_channels[first_index].friendly_name,
          upd.local_channel_indices.len() - 1
        );
        debug!("{upd:?}");
        let mut flows = self.flows.lock().unwrap();
        let mut subinfos = self.subscriptions_info.write().unwrap();
        for chi in upd.local_channel_indices {
          let subscription = self.channels[chi].as_mut().unwrap();
          let remote = upd.remote.clone();
          let flow = flows[remote.local_flow_index].as_mut().unwrap();
          flow.tx_channels[remote.channel_in_flow] = Some(remote.tx_channel_id);
          flow.channels_refcount[remote.channel_in_flow] += 1;
          if flow.channels_rb_outputs[remote.channel_in_flow].is_none() {
            // ringbuffer doesn't exist yet (this channel in flow wasn't used before)
            let local_id = flow.local_id;
            debug_assert!(local_id == remote.local_flow_index+1);
          }
          let latency_samples = flow.latency_samples;
          let sink_opt = self.channels_buffering.connect_channel(now /* FIXME shift */, &mut flow.channels_rb_outputs[remote.channel_in_flow], chi, latency_samples);
          if let Some(sink) = sink_opt {
            let flows_recv = self.flows_recv.clone();
            tokio::spawn(async move {
              flows_recv.connect_channel(remote.local_flow_index, remote.channel_in_flow, sink, latency_samples).await;
            });
          }
          subscription.remote = Some(remote);
          subinfos[chi] = Some(SubscriptionInfo {
            tx_channel_name: subscription.tx_channel_name.clone(),
            tx_hostname: subscription.tx_hostname.clone(),
            status: SubscriptionStatus::InProgress
          });
          changed_channels.push(chi);
          // TODO probably some lines can be brought out of this loop
        }
      }
      self.notify_channels_change(changed_channels).await;
      trace!("recv iter");
    }
    trace!("recv loop exited");
    return true;
  }

  /* fn get_ring_buffer(&self, local_channel_index: usize) -> (RBInput<Sample, P>, Option<RBOutput<Sample, P>>) {

  } */

  async fn scan_flows(&mut self) -> bool {
    let mut destroy_futures_per_remote: BTreeMap<
      SocketAddr,
      Vec<Pin<Box<dyn Future<Output = ()> + Send>>>,
    > = BTreeMap::new();
    let mut channels_changed = vec![];
    {
      let mut flows = self.flows.lock().unwrap();
      let min_last_packet_time = self.ref_instant.elapsed().as_secs().saturating_sub(2) as _;
      let min_flow_creation_time = self.ref_instant.elapsed().as_secs().saturating_sub(6) as _;
      let flows_to_remove = flows
        .iter_mut()
        .enumerate()
        .filter_map(|(flow_index, opt)| {
          match opt {
            None => None,
            Some(flow) => {
              let mut remove = false;
              let mut receiving = false;
              if flow.is_used() {
                if flow.last_packet_time.load(Ordering::Relaxed) < min_last_packet_time {
                  // but do this only if the flow is old enough
                  if flow.creation_time < min_flow_creation_time {
                    warn!("flow index={flow_index} timeout (not receiving media packets)");
                    remove = true;
                  }
                } else {
                  receiving = true;
                }
                if remove || flow.needs_subscription_info_update {
                  for (chi, ch) in self.channels.iter_mut().enumerate() {
                    if let Some(chsub) = ch {
                      if chsub.remote.is_some()
                        && chsub.remote.as_ref().unwrap().local_flow_index == flow_index
                      {
                        if remove {
                          chsub.remote = None;
                          self.needs_resolving = true;
                          self.resolve_now = true;
                          warn!(
                            "channel subscribed to {}@{} is orphaned now",
                            chsub.tx_channel_name, chsub.tx_hostname
                          );
                          if let Some(subi) = self.subscriptions_info.write().unwrap()[chi].as_mut()
                          {
                            subi.status = SubscriptionStatus::Unresolved;
                            channels_changed.push(chi);
                          }
                          self.channels_buffering.disconnect_channel(chi);
                        } else if receiving {
                          if let Some(subi) = self.subscriptions_info.write().unwrap()[chi].as_mut()
                          {
                            let (status, desc) = match flow.source {
                              FlowSource::Unicast(_) => (SubscriptionStatus::ReceivingUnicast, "unicast"),
                              FlowSource::Multicast(_) => (SubscriptionStatus::ReceivingMulticast, "multicast"),
                            };
                            if subi.status != status {
                              info!(
                                "channel index={chi}, flow index={flow_index} is receiving data ({desc})"
                              );
                              channels_changed.push(chi);
                            }
                            subi.status = status;
                            //flow.needs_subscription_info_update = false;
                            // for now assume that we always need updating
                            // TODO figure it out with aliases
                          }
                        }
                      }
                    }
                  }
                }
              } else {
                debug!("flow index={flow_index} is not in use");
                remove = true;
              }
              if remove {
                match &flow.source {
                  FlowSource::Unicast(uf) => {
                    let control_client = self.control_client.clone();
                    let flows_recv = self.flows_recv.clone();
                    let rem_addr = uf.control_remote_addr;
                    let dbcp1 = uf.dbcp1;
                    let handle = uf.handle.unwrap();
                    let flow_id = flow.local_id;
                    destroy_futures_per_remote.entry(uf.control_remote_addr).or_default().push(
                      async move {
                        info!("removing no longer needed flow index={flow_index}, remote={rem_addr:?}");
                        control_client.stop_flow(&rem_addr, dbcp1, handle).await.log_and_forget();
                        // we can safely ignore errors - we remove the socket so keepalive messages won't arrive to the tx so it'll kick us off anyway
                        flows_recv.remove_socket(flow_index).await;
                      }
                      .boxed(),
                    );
                  },
                  FlowSource::Multicast(mf) => {
                    let flows_recv = self.flows_recv.clone();
                    tokio::spawn(async move {
                      flows_recv.remove_socket(flow_index).await;
                    });
                  },
                }

                Some(flow_index)
              } else {
                None
              }
            }
          }
        })
        .collect_vec();
      for &flow_index in &flows_to_remove {
        flows[flow_index] = None;
      }
    }
    self.notify_channels_change(channels_changed).await;
    join_all(destroy_futures_per_remote.iter_mut().map(|(_, ops)| async move {
      for op in ops {
        op.await;
      }
    }))
    .await;
    return false;
  }

  async fn handle_command(&mut self, command: Command) -> bool {
    match command {
      Command::Shutdown => return false,
      Command::Subscribe { local_channel_index, tx_channel_name, tx_hostname } => {
        self.subscribe(local_channel_index, &tx_channel_name, &tx_hostname).await;
      }
      Command::Unsubscribe { local_channel_index } => {
        self.unsubscribe(local_channel_index, true).await;
      }
      Command::ForceRefresh => {}
    };
    return true;
  }
  pub async fn run(&mut self) {
    self.load_state().await;
    let mut last_resolving = Instant::now();
    loop {
      // TODO make it more NOHZ
      let mut received = match timeout(Duration::from_secs(2), self.commands_receiver.recv()).await
      {
        Ok(Some(command)) => Some(command),
        Ok(None) => break,
        Err(_) => None,
      };
      let mut received_command = false;
      while let Some(command) = received {
        if !self.handle_command(command).await {
          return;
        }
        received_command = true;
        received = match self.commands_receiver.try_recv() {
          Ok(v) => Some(v),
          Err(_) => None,
        };
      }
      if received_command {
        // received subscribe or unsubscribe command, which means that state needs to be saved
        self.save_state();
      }
      if (self.needs_resolving && last_resolving.elapsed() >= Duration::from_secs(9))
        || self.resolve_now
      {
        self.resolve_subscriptions().await;
        last_resolving = Instant::now();
      }
      self.scan_flows().await;
    }
  }
  fn save_state(&self) {
    let state = SavedChannelsState { channels:
      self.channels.iter().enumerate().filter_map(|(index, sub)| {
        match sub {
          None => None,
          Some(sub) => Some(SavedChannelState {
            local_channel_id: (index+1) as u16,
            local_channel_name: self.self_info.rx_channels[index].friendly_name.clone(),
            tx_channel_name: sub.tx_channel_name.clone(),
            tx_hostname: sub.tx_hostname.clone()
          })
        }
      }).collect_vec()
    };
    self.state_storage.save("channels", &state).log_and_forget();
  }
  async fn load_state(&mut self) {
    let channels = {
      let result = self.state_storage.load::<SavedChannelsState>("channels");
      if let Err(e) = result {
        warn!("Unable to load state (this is normal in first run): {e:?}");
        return;
      }
      result.unwrap().channels
    };
    for chst in channels {
      let lci = chst.local_channel_id as usize-1;
      if lci >= self.channels.len() {
        warn!("loading state containing channel id {} while we have {} channels. State of this channel will be lost on next save.", chst.local_channel_id, self.channels.len());
        continue;
      }
      self.subscriptions_info.write().unwrap()[lci] = Some(SubscriptionInfo {
        tx_hostname: chst.tx_hostname.clone(),
        tx_channel_name: chst.tx_channel_name.clone(),
        status: SubscriptionStatus::Unresolved,
      });
      self.subscribe(lci, &chst.tx_channel_name, &chst.tx_hostname).await;
    }
  }
}

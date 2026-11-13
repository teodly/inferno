use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::num::Wrapping;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::{
  collections::BTreeMap,
  net::SocketAddr,
  sync::atomic::AtomicU32,
  time::{Duration, Instant},
};

use atomic::Ordering;
use futures::FutureExt;
use itertools::Itertools;
use rand::rngs::SmallRng;
use rand::{thread_rng, Rng, SeedableRng};
use tokio::sync::watch;
use tokio::{select, sync::mpsc};

use super::samples_utils::*;
use super::tx_multicasts::MEDIA_PORT;
use crate::device_server::{NotifyThrottle, TransferNotifier};
use crate::media_clock::async_clock_receiver_to_realtime;
use crate::ring_buffer::{ProxyToSamplesBuffer, RBOutput};
use crate::util::os::set_current_thread_realtime;
use crate::util::real_time_box_channel::RealTimeBoxReceiver;
use crate::util::thread::run_future_in_new_thread;
use crate::{
  common::Sample,
  media_clock::{ClockOverlay, MediaClock},
  net_utils::MTU,
  protocol::flows_control::FlowHandle,
};
use crate::{common::*, device_info::DeviceInfo};

pub const FPP_MIN: u16 = 2;
pub const FPP_MAX: u16 = 256;
pub const FPP_MAX_ADVERTISED: u16 = 32;
pub const MAX_FLOWS: u32 = 32;
pub const MAX_CHANNELS_IN_FLOW: u16 = 8;
pub const KEEPALIVE_TIMEOUT_SECONDS: Clock = 4;
pub const DISCONTINUITY_THRESHOLD_SAMPLES: usize = 192000;
const BUFFERED_SAMPLES_PER_CHANNEL: usize = 65536;
pub const SELECT_THRESHOLD: Duration = Duration::from_millis(100);
pub const PROCESS_EVENTS_INTERVAL: Duration = Duration::from_millis(33);
pub const MIN_SLEEP: Duration = Duration::from_millis(0); // to save CPU cycles, TODO: make it configurable via some "eco mode" flag

// it's better to have the clock in the past than in the future - otherwise Dante devices receiving from us go mad and fart
const CLOCK_OFFSET_NS: ClockDiff = -500_000;

pub type SamplesRequestCallback = Box<dyn FnMut(Clock, usize, &mut [Sample]) + Send + 'static>;

struct Flow {
  socket: UdpSocket,
  channel_indices: Vec<Option<usize>>,
  next_ts: LongClock,
  fpp: usize,
  bytes_per_sample: usize,
  expires: Option<Clock>,
  expired: Arc<AtomicBool>,
}

impl Flow {
  fn bootstrap_next_ts(&mut self, now: LongClock) {
    let remainder = now % (self.fpp as LongClock);
    self.next_ts = now.wrapping_add(self.fpp as LongClock - remainder);
  }
  fn keep_alive(&mut self, now: Clock, sample_rate: u32) {
    self.expires.as_mut().map(|expires| {
      *expires = now.wrapping_add(KEEPALIVE_TIMEOUT_SECONDS * sample_rate as Clock);
    });
  }
}

#[derive(Debug)]
enum Command {
  NoOp,
  Shutdown,
  AddFlow {
    index: usize,
    socket: UdpSocket,
    channel_indices: Vec<Option<usize>>,
    fpp: usize,
    bytes_per_sample: usize,
    needs_keepalives: bool,
    expired: Arc<AtomicBool>,
  },
  RemoveFlow {
    index: usize,
  },
  SetChannels {
    index: usize,
    channel_indices: Vec<Option<usize>>,
  },
}

struct FlowsTransmitterInternal<P: ProxyToSamplesBuffer> {
  commands_receiver: mpsc::Receiver<Command>,
  clock_recv: RealTimeBoxReceiver<Option<ClockOverlay>>,
  sample_rate: u32,
  flows: Vec<Option<Flow>>,
  clock: MediaClock,
  channels_sources: Vec<RBOutput<Sample, P>>,
  send_latency_samples: usize,
  clock_offset_samples: LongClockDiff,
  max_lag_samples: usize,
  timestamp_shift: ClockDiff,
  tx_source_bit_depth: u8,
  current_timestamp: Arc<AtomicUsize>,
  on_transfer: Option<TransferNotifier>,
  //callback: SamplesRequestCallback,
}

impl<P: ProxyToSamplesBuffer> FlowsTransmitterInternal<P> {
  #[inline(always)]
  fn should_dither(&self, output_bit_depth: u8) -> bool {
    self.tx_source_bit_depth > output_bit_depth
  }

  fn now(&self) -> Option<LongClock> {
    self.clock.now_in_timebase(self.sample_rate as u64)
  }

  #[inline(always)]
  fn transmit(&mut self, dither_rng: &mut SmallRng, now: LongClock, process_events: bool) {
    let mut tmp_samples = [0 as Sample; FPP_MAX as usize];
    let mut pbuff = [0u8; MTU];
    let sample_rate = self.sample_rate;
    let max_awake_time_samples = (self.sample_rate / 200) as ClockDiff;
    let max_lag_samples = self.max_lag_samples;
    let mut iterations = 0;
    let mut max_missing_samples = 0;
    let dither_16 = self.should_dither(16);
    let dither_24 = self.should_dither(24);
    for flow in &mut self.flows.iter_mut().filter_map(|opt| opt.as_mut()) {
      if flow.expired.load(Ordering::Relaxed) {
        continue;
      }
      let channels_in_flow = flow.channel_indices.len();
      let stride = channels_in_flow * flow.bytes_per_sample;
      let lag = wrapped_diff(now as Clock, flow.next_ts as Clock);
      if lag > max_lag_samples as ClockDiff {
        error!("tx lag of {} samples detected, or media clock jumped, dropout occurs!", lag);
        flow.bootstrap_next_ts(now);
      }
      if lag < -(DISCONTINUITY_THRESHOLD_SAMPLES as ClockDiff) {
        error!("media clock jumped: {}", lag);
        flow.bootstrap_next_ts(now);
      }
      pbuff[9..9 + stride * flow.fpp].fill(0);
      while wrapped_diff(now as Clock, flow.next_ts as Clock) >= 0 {
        pbuff[0] = 2u8; // ???
        let packet_ts = flow.next_ts.wrapping_add_signed(self.clock_offset_samples) /* .wrapping_sub(flow.fpp) */; // ???
        let seconds = packet_ts / (sample_rate as LongClock);
        let subsec_samples = packet_ts % (sample_rate as LongClock);
        pbuff[1..5].copy_from_slice(&(seconds as u32).to_be_bytes());
        pbuff[5..9].copy_from_slice(&(subsec_samples as u32).to_be_bytes());
        let start_ts = (flow.next_ts as Clock).wrapping_add_signed(self.timestamp_shift);
        for (index_in_flow, &ch_opt) in flow.channel_indices.iter().enumerate() {
          if let Some(ch_index) = ch_opt {
            //(self.callback)(flow.next_ts, ch_index, &mut tmp_samples[0..flow.fpp]);
            // TODO remove not really necessary copy to tmp_samples, write_*_samples could read directly from ring buffer
            let r =
              self.channels_sources[ch_index].read_at(start_ts as usize, &mut tmp_samples[0..flow.fpp]);
            if r.useful_start_index != 0 || r.useful_end_index != flow.fpp {
              /* error!(
                  "didn't have enough samples, transmitting silence. {} {}",
                  r.useful_start_index,
                  flow.fpp - r.useful_end_index
              ); */

              tmp_samples[0..r.useful_start_index].fill(0);
              tmp_samples[r.useful_end_index..].fill(0);

              let missing_samples = r.useful_start_index + flow.fpp - r.useful_end_index;
              max_missing_samples = max_missing_samples.max(missing_samples);
            }
            let start = 9 + index_in_flow * flow.bytes_per_sample;
            let samples = &tmp_samples[0..flow.fpp];
            match flow.bytes_per_sample {
              2 => write_s16_samples::<_, SmallRng>(
                samples,
                &mut pbuff,
                start,
                stride,
                if dither_16 { Some(dither_rng) } else { None },
              ),
              3 => write_s24_samples::<_, SmallRng>(
                samples,
                &mut pbuff,
                start,
                stride,
                if dither_24 { Some(dither_rng) } else { None },
              ),
              4 => write_s32_samples::<_, SmallRng>(samples, &mut pbuff, start, stride, None),
              other => {
                error!("BUG: unsupported bytes per sample {}", other);
              }
            }
          }
        }
        let to_send = 9 + stride * flow.fpp;
        if let Ok(written) = flow.socket.send(&pbuff[0..to_send]) {
          if written == to_send {
            flow.next_ts = flow.next_ts.wrapping_add(flow.fpp.try_into().unwrap());
          } else {
            warn!("written {written}, should have {to_send}");
          }
        } else {
          warn!("send returned error");
        }
        iterations += 1;
        if (iterations % 16) == 0 {
          if let Some(real_now) = self.clock.wrapping_now_in_timebase(self.sample_rate as u64) {
            let diff = wrapped_diff(real_now.try_into().unwrap(), now as Clock);
            if diff > max_awake_time_samples {
              warn!("blocked for {diff} samples, yielding to avoid CPU lockup");
              std::thread::sleep(Duration::from_micros(2000));
              return;
            }
          }
        }
      }
      if process_events && flow.expires.is_some() {
        if let Ok(_) = flow.socket.recv(&mut pbuff) {
          flow.keep_alive(now as Clock, sample_rate);
        } else if wrapped_diff(flow.expires.unwrap(), now as Clock) < 0 {
          flow.expired.store(true, Ordering::Release);
          info!("flow dst {:?} expired (no keepalives received)", flow.socket.peer_addr().ok());
        }
      }
    }
  }

  async fn run(&mut self, mut start_time_rx: Option<tokio::sync::oneshot::Receiver<Clock>>) {
    let sample_rate = self.sample_rate;
    let process_events_interval = (sample_rate / 30) as Clock;
    let mut dither_rng = SmallRng::from_rng(rand::thread_rng()).unwrap();

    if let Some(rx) = &mut start_time_rx {
      match rx.await {
        Ok(start_time) => {
          self.timestamp_shift = (0 as ClockDiff)
            .wrapping_sub_unsigned(start_time)
            .wrapping_sub_unsigned(self.send_latency_samples.try_into().unwrap());
        }
        Err(e) => {
          error!("unable to get start timestamp for ring buffer output: {e:?}");
          return;
        }
      }
    }

    let now = loop {
      self.clock_recv.update();
      if let Some(clkovl) = self.clock_recv.get() {
        let had_clock = self.clock.is_ready(); // TODO simplify
        self.clock.update_overlay(*clkovl);
        if !had_clock {
          let now = self.now().unwrap();
          for flow in &mut self.flows.iter_mut().filter_map(|opt| opt.as_mut()) {
            flow.bootstrap_next_ts(now);
            flow.keep_alive(now as Clock, self.sample_rate);
          }
        }
      }
      let now_opt = self.now();
      if let Some(now) = now_opt {
        break now;
      } else {
        error!("clock unavailable, can't transmit. is the PTP daemon running? (@init)");
        tokio::time::sleep(Duration::from_secs(1)).await;
      }
    };
    let mut next_on_transfer = now as Clock;
    let mut notify =
      self.on_transfer.as_ref().map(|t| NotifyThrottle::new(t.max_interval_samples, sample_rate as u32));
    let mut next_process_events = now as Clock;
    let _ = now;

    let _ = set_current_thread_realtime(81);
    loop {
      let min_next_ts = self
        .flows
        .iter()
        .filter_map(|opt| opt.as_ref())
        .filter(|flow| !flow.expired.load(Ordering::Relaxed))
        .map(|&ref flow| flow.next_ts as Clock)
        .min_by(|&a, &b| wrapped_diff(a, b).cmp(&0));

      let sleep_until =
        [min_next_ts, if self.on_transfer.is_some() { Some(next_on_transfer) } else { None }]
          .into_iter()
          .filter_map(|opt| opt)
          .min_by(|&a, &b| wrapped_diff(a as Clock, b as Clock).cmp(&0));

      if self.clock_recv.update() {
        if let Some(ovl) = self.clock_recv.get() {
          self.clock.update_overlay(*ovl);
        }
      }
      let now = if let Some(now) = self.now() {
        now
      } else {
        error!("clock unavailable, can't transmit. is the PTP daemon running? (@get now)");
        tokio::time::sleep(Duration::from_secs(1)).await;
        continue;
      };

      let sleep_duration = sleep_until
        .and_then(|ts| self.clock.system_clock_duration_from_until(now as Clock, ts, sample_rate as u64))
        .unwrap_or(std::time::Duration::from_secs(20))
        .max(MIN_SLEEP);

      let command = if sleep_duration < SELECT_THRESHOLD {
        // on_transfer callback must be called to notify about transmission in previous iteration,
        // after updating current timestamp, before waiting
        let cur_ts_opt =
          min_next_ts.map(|n| n as usize).map(|n| if n == usize::MAX { usize::MAX - 1 } else { n });
        self
          .current_timestamp
          .store(cur_ts_opt.unwrap_or(usize::MAX), Ordering::SeqCst /*TODO: really needed?*/);
        // Rate limited: this branch runs once per packet. (The branch below
        // precedes a long wait, so it always notifies.)
        if let Some(transfer) = self.on_transfer.as_ref() {
          if notify.as_mut().is_some_and(|n| n.due(Instant::now())) {
            (transfer.callback)();
          }
        }

        if !sleep_duration.is_zero() {
          std::thread::sleep(sleep_duration);
        }
        if let Some(now) = self.now() {
          let process_events = wrapped_diff(now as Clock, next_process_events) >= 0;
          self.transmit(&mut dither_rng, now, process_events);
          if process_events {
            next_process_events = (now as Clock).wrapping_add(process_events_interval);
            self.commands_receiver.try_recv().unwrap_or(Command::NoOp)
          } else {
            Command::NoOp
          }
        } else {
          error!("clock unavailable, can't transmit. is the PTP daemon running? (@non-select)");
          self.commands_receiver.try_recv().unwrap_or(Command::NoOp)
        }
      } else {
        // on_transfer callback must be called to notify about transmission in previous iteration,
        // after updating current timestamp, before waiting
        self.current_timestamp.store(usize::MAX, Ordering::SeqCst);
        self.on_transfer.as_ref().map(|transfer| (transfer.callback)());

        select! {
          recv_opt = self.commands_receiver.recv() => {
            recv_opt.unwrap_or(Command::Shutdown)
          },
          _ = tokio::time::sleep(sleep_duration) => {
            if let Some(now) = self.now() {
              self.transmit(&mut dither_rng, now, true);
            } else {
              error!("clock unavailable, can't transmit. is the PTP daemon running? (@select)");
            }
            Command::NoOp
          }
        }
      };

      let now_opt = if self.on_transfer.is_some() { self.now() } else { None };
      if let Some(transfer) = self.on_transfer.as_ref() {
        if let Some(now) = now_opt {
          if wrapped_diff(next_on_transfer, now as Clock) <= 0 {
            // TODO: /2 is a HACK
            next_on_transfer = next_on_transfer.wrapping_add(transfer.max_interval_samples / 2);
            let diff = wrapped_diff(next_on_transfer, now as Clock);
            if diff < 0 || diff > (transfer.max_interval_samples * 2).try_into().unwrap() {
              warn!("clock jumped, sanitizing next_on_transfer");
              next_on_transfer = (now as Clock).wrapping_add(transfer.max_interval_samples);
            }
          } else {
            next_on_transfer = (now as Clock).wrapping_add(transfer.max_interval_samples);
          }
        } else {
          error!(
            "clock unavailable, can't set next transfer notification time. is the PTP daemon running?"
          );
        }
      }

      match command {
        Command::Shutdown => {
          break;
        }
        Command::AddFlow {
          index,
          socket,
          channel_indices,
          fpp,
          bytes_per_sample,
          needs_keepalives,
          expired,
        } => {
          let mut flow = Flow {
            socket,
            channel_indices,
            next_ts: 0,
            fpp,
            bytes_per_sample,
            expires: if needs_keepalives { Some(0) } else { None },
            expired,
          };
          if let Some(now) = self.now() {
            flow.bootstrap_next_ts(now);
            if needs_keepalives {
              flow.keep_alive(now as Clock, self.sample_rate);
            }
          }
          let previous = std::mem::replace(&mut self.flows[index], Some(flow));
          debug_assert!(previous.is_none());
        }
        Command::RemoveFlow { index } => {
          self.flows[index] = None; // TODO is freeing memory in realtime thread safe???
        }
        Command::SetChannels { index, channel_indices } => {
          let now_opt = self.now();
          let flow = self.flows[index].as_mut().unwrap();
          flow.channel_indices = channel_indices;
          if let Some(now) = now_opt {
            flow.keep_alive(now as Clock, self.sample_rate);
          }
          if flow.expired.load(Ordering::Relaxed) {
            info!("resuscitating expired flow index={index}");
            if let Some(now) = now_opt {
              flow.bootstrap_next_ts(now);
            }
            flow.expired.store(false, Ordering::Release);
          }
        }
        Command::NoOp => {}
      }
    }
  }
}

struct FlowData {
  cookie: u16,
  remote: SocketAddr,
  expired: Arc<AtomicBool>,
  // the TX thread's packet layout, needed to validate later channel changes
  fpp: usize,
  bytes_per_sample: usize,
}

/// Media packet header (type byte, seconds, subsecond samples) before the samples.
const MEDIA_HEADER_BYTES: usize = 9;

/// Why a flow (requested over the network, or created for multicast) cannot be sent.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FlowLayoutError {
  #[error("unsupported bytes per sample: {0}")]
  BytesPerSample(usize),
  #[error("frames per packet {0} outside {FPP_MIN}..={FPP_MAX}")]
  FramesPerPacket(usize),
  #[error("channel index {index} but only {channels} TX channels")]
  ChannelIndex { index: usize, channels: usize },
  #[error("packet of {0} bytes does not fit the {MTU}-byte buffer")]
  PacketTooLarge(usize),
}

/// Checks a flow before it reaches the TX thread, which slices its MTU-sized
/// packet buffer with `channels * bytes_per_sample * fpp`, indexes its channel
/// sources with the channel indices and advances each flow by `fpp` frames per
/// packet (0 would never advance): all of these come from network requests.
/// The channel count itself is bounded only by the packet size.
pub fn validate_flow_layout(
  channel_indices: &[Option<usize>],
  fpp: usize,
  bytes_per_sample: usize,
  num_channels: usize,
) -> Result<(), FlowLayoutError> {
  if !(2..=4).contains(&bytes_per_sample) {
    return Err(FlowLayoutError::BytesPerSample(bytes_per_sample));
  }
  if !(FPP_MIN as usize..=FPP_MAX as usize).contains(&fpp) {
    return Err(FlowLayoutError::FramesPerPacket(fpp));
  }
  if let Some(&index) = channel_indices.iter().flatten().find(|&&i| i >= num_channels) {
    return Err(FlowLayoutError::ChannelIndex { index, channels: num_channels });
  }
  let packet_bytes = MEDIA_HEADER_BYTES + channel_indices.len() * bytes_per_sample * fpp;
  if packet_bytes > MTU {
    return Err(FlowLayoutError::PacketTooLarge(packet_bytes));
  }
  Ok(())
}

fn invalid_input(e: FlowLayoutError) -> std::io::Error {
  error!("rejecting flow: {e}");
  std::io::Error::new(std::io::ErrorKind::InvalidInput, e)
}

#[derive(Debug, Clone)]
pub struct FlowInfo {
  pub rx_hostname: Option<String>,
  pub rx_flow_name: Option<String>,
  pub dst_addr: Ipv4Addr,
  pub dst_port: u16,
  pub local_channel_indices: Vec<Option<usize>>,
}

impl FlowInfo {
  fn is_multicast(&self) -> bool {
    self.rx_hostname.is_none() && self.rx_flow_name.is_none()
  }
}

/// A unicast flow as it stood when its transmitter stopped. The next
/// transmitter re-creates it with the same index and cookie (see
/// FlowsTransmitter::restore), so the handle a receiver holds stays valid
/// across a transmitter restart.
#[derive(Debug, Clone)]
pub struct SavedFlow {
  index: u32,
  cookie: u16,
  remote: SocketAddr,
  fpp: usize,
  bytes_per_sample: usize,
  info: FlowInfo,
}

pub struct FlowsTransmitter {
  self_info: Arc<DeviceInfo>,
  flow_seq_id: AtomicU32,
  flows: BTreeMap<u32, FlowData>,
  ip_port_to_id: BTreeMap<SocketAddr, u32>,
  commands_sender: mpsc::Sender<Command>,
  flows_info: Vec<Option<FlowInfo>>,
  // number of channel sources the TX thread reads from
  num_channels: usize,
}

fn split_handle(h: FlowHandle) -> (u32, u16) {
  (u32::from_be_bytes(h[0..4].try_into().unwrap()), u16::from_be_bytes(h[4..6].try_into().unwrap()))
}

impl FlowsTransmitter {
  async fn run<P: ProxyToSamplesBuffer>(
    rx: mpsc::Receiver<Command>,
    tx_source_bit_depth: u8,
    clock_recv: RealTimeBoxReceiver<Option<ClockOverlay>>,
    sample_rate: u32,
    latency_ns: usize,
    max_lag_samples: usize,
    channels_outputs: Vec<RBOutput<Sample, P>>,
    start_time_rx: Option<tokio::sync::oneshot::Receiver<Clock>>,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
  ) {
    let latency: u32 = (latency_ns as u64 * sample_rate as u64 / 1_000_000_000u64).try_into().unwrap();
    let mut internal = FlowsTransmitterInternal {
      commands_receiver: rx,
      clock_recv,
      sample_rate,
      flows: (0..MAX_FLOWS).map(|_| None).collect_vec(),
      clock: MediaClock::new(false /* TODO */),
      channels_sources: channels_outputs,
      send_latency_samples: latency.try_into().unwrap(), // TODO in ALSA plugin should be 0, the more the worse because aplay wants to fill the whole buffer
      max_lag_samples,
      timestamp_shift: (0 as ClockDiff).wrapping_sub_unsigned(latency.try_into().unwrap()),
      tx_source_bit_depth,
      clock_offset_samples: (CLOCK_OFFSET_NS as i64 * sample_rate as i64 / 1_000_000_000i64)
        .try_into()
        .unwrap(),
      current_timestamp,
      on_transfer,
    };
    internal.run(start_time_rx).await;
  }
  pub fn start<P: ProxyToSamplesBuffer + Send + Sync + 'static>(
    self_info: Arc<DeviceInfo>,
    tx_latency_ns: usize,
    tx_source_bit_depth: u8,
    clock_recv: RealTimeBoxReceiver<Option<ClockOverlay>>,
    channels_outputs: Vec<RBOutput<Sample, P>>,
    start_time_rx: Option<tokio::sync::oneshot::Receiver<Clock>>,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
  ) -> (Self, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(100);
    let tx1 = tx.clone();
    let srate = self_info.sample_rate;
    let num_channels = channels_outputs.len();
    // TODO dehardcode latency_ns
    let thread_join = run_future_in_new_thread("flows TX", move || {
      Self::run(
        rx,
        tx_source_bit_depth,
        clock_recv,
        srate,
        0, /*LATENCY TODO*/
        // we set max_lag_samples to tx latency because it doesn't make sense to send samples older than that
        (tx_latency_ns as u64 * srate as u64 / 1_000_000_000u64).try_into().unwrap(),
        channels_outputs,
        start_time_rx,
        current_timestamp,
        on_transfer,
      )
      .boxed_local()
    });
    return (
      Self {
        commands_sender: tx,
        self_info: self_info.clone(),
        flow_seq_id: 0.into(),
        flows: BTreeMap::new(),
        ip_port_to_id: BTreeMap::new(),
        flows_info: (0..MAX_FLOWS).map(|_| None).collect_vec(),
        num_channels,
      },
      thread_join,
    );
  }
  pub async fn shutdown(&self) {
    self.commands_sender.send(Command::Shutdown).await.log_and_forget();
  }

  pub fn destination_exists(&self, dst_addr: Ipv4Addr, dst_port: u16) -> bool {
    let socket_addr = SocketAddr::new(IpAddr::V4(dst_addr), dst_port);
    self.ip_port_to_id.contains_key(&socket_addr)
  }
  pub async fn add_flow(
    &mut self,
    flow_info: FlowInfo,
    fpp: usize,
    bytes_per_sample: usize,
    requested_flow_index: Option<u32>,
    is_multicast: bool,
  ) -> Result<(usize, FlowHandle), std::io::Error> {
    let channel_indices = flow_info.local_channel_indices.clone();
    if let Some(index) = requested_flow_index {
      if index >= MAX_FLOWS {
        error!("requested flow index out of range: {index}");
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
      }
    }
    let dst_addr = SocketAddr::new(IpAddr::V4(flow_info.dst_addr), flow_info.dst_port);
    let (flow_index, cookie) = match self.ip_port_to_id.get(&dst_addr) {
      None => {
        validate_flow_layout(&channel_indices, fpp, bytes_per_sample, self.num_channels)
          .map_err(invalid_input)?;
        self.scan_expired().await;
        let mut counter = 0;
        let flow_index = loop {
          let flow_index = requested_flow_index
            .unwrap_or_else(|| self.flow_seq_id.fetch_add(1, atomic::Ordering::AcqRel) % MAX_FLOWS);
          if !self.flows.contains_key(&flow_index) {
            if requested_flow_index.is_some() && self.flows_info[flow_index as usize].is_some() {
              error!("requested flow index which is reserved: {flow_index}");
              return Err(std::io::Error::from(std::io::ErrorKind::ResourceBusy));
            } else {
              break flow_index;
            }
          }
          if requested_flow_index.is_some() {
            error!("requested flow index which is already in use: {flow_index}");
            return Err(std::io::Error::from(std::io::ErrorKind::ResourceBusy));
          }
          counter += 1;
          if counter > MAX_FLOWS {
            error!("ran out of flows! {MAX_FLOWS}");
            return Err(std::io::Error::from(std::io::ErrorKind::OutOfMemory));
          }
        };
        let flow = FlowData {
          cookie: thread_rng().gen(),
          remote: dst_addr.clone(),
          // we're adding multicast flow as 'expired' to give it grace period for multicast address collission detection
          expired: Arc::new(AtomicBool::new(is_multicast)),
          fpp,
          bytes_per_sample,
        };

        let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(self.self_info.ip_address), 0))?;
        socket.connect(dst_addr)?;
        socket.set_nonblocking(true)?;
        //socket.set_read_timeout(Some(Duration::from_micros(1)))?;

        self
          .commands_sender
          .send(Command::AddFlow {
            index: flow_index as usize,
            socket,
            channel_indices: channel_indices.clone(),
            fpp,
            bytes_per_sample,
            needs_keepalives: !is_multicast,
            expired: flow.expired.clone(),
          })
          .await
          .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;

        let cookie = flow.cookie;
        self.flows.insert(flow_index, flow);
        (flow_index, cookie)
      }
      Some(&flow_index) => {
        warn!("got add flow request for already existing flow, setting channels instead");
        // TODO FIXME what if fpp or bytes_per_sample change?
        // the TX thread keeps the existing flow's layout, so validate against that
        let existing = self.flows.get(&flow_index).ok_or(std::io::ErrorKind::NotFound)?;
        validate_flow_layout(
          &channel_indices,
          existing.fpp,
          existing.bytes_per_sample,
          self.num_channels,
        )
        .map_err(invalid_input)?;
        let cookie = existing.cookie;
        self
          .commands_sender
          .send(Command::SetChannels {
            index: flow_index as usize,
            channel_indices: channel_indices.clone(),
          })
          .await
          .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        (flow_index, cookie)
      }
    };

    self.ip_port_to_id.insert(dst_addr, flow_index);

    let mut flow_handle = [0u8; 6];
    flow_handle[0..4].copy_from_slice(&flow_index.to_be_bytes());
    flow_handle[4..6].copy_from_slice(&cookie.to_be_bytes());

    self.flows_info[flow_index as usize] = Some(flow_info);

    Ok((flow_index as usize, flow_handle))
  }
  /// Called for multicast flows after the grace period. Returns false when the
  /// flow no longer exists (it was deleted during the grace period).
  pub fn activate_multicast_flow(&mut self, flow_index: u32) -> bool {
    match self.flows.get(&flow_index) {
      Some(flow) => {
        flow.expired.store(false, Ordering::Release);
        true
      }
      None => false,
    }
  }
  pub fn random_multicast_destination(&self) -> (Ipv4Addr, u16) {
    loop {
      let port = MEDIA_PORT;
      let ip = Ipv4Addr::new(239, 255, thread_rng().gen(), thread_rng().gen());
      if !self.destination_exists(ip, port) {
        return (ip, port);
      }
    }
  }

  fn get_flow(&self, handle: FlowHandle) -> Option<(u32, &FlowData)> {
    let (id, cookie) = split_handle(handle);
    self.flows.get(&id).filter(|flow| flow.cookie == cookie).map(|flow| (id, flow))
  }
  async fn remove_flow_internal(&mut self, index: u32) {
    if let Some(flow) = self.flows.remove(&index) {
      self.ip_port_to_id.remove(&flow.remote);
    }
    self.flows_info[index as usize] = None;
    self.commands_sender.send(Command::RemoveFlow { index: index as usize }).await.unwrap();
  }
  pub async fn remove_flow(&mut self, handle: FlowHandle) -> Result<usize, std::io::Error> {
    if let Some((id, _)) = self.get_flow(handle) {
      self.remove_flow_internal(id).await;
      Ok(id as usize)
    } else {
      Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }
  }
  pub async fn remove_multicast_flow(&mut self, index: u32) -> Result<(), std::io::Error> {
    if let Some(Some(info)) = self.flows_info.get(index as usize).as_ref() {
      if info.rx_flow_name.is_none() && info.rx_hostname.is_none() {
        self.remove_flow_internal(index).await;
        Ok(())
      } else {
        error!("trying to remove non-multicast flow which can be only managed using a handle");
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
      }
    } else {
      Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }
  }
  pub async fn set_channels(
    &mut self,
    handle: FlowHandle,
    channel_indices: impl IntoIterator<Item = Option<usize>>,
  ) -> Result<usize, std::io::Error> {
    if let Some((index, flow)) = self.get_flow(handle) {
      let channel_indices = channel_indices.into_iter().collect_vec();
      validate_flow_layout(&channel_indices, flow.fpp, flow.bytes_per_sample, self.num_channels)
        .map_err(invalid_input)?;
      self
        .commands_sender
        .send(Command::SetChannels { index: index as usize, channel_indices: channel_indices.clone() })
        .await
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;

      self.flows_info[index as usize].as_mut().unwrap().local_channel_indices = channel_indices;
      Ok(index as usize)
    } else {
      Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    }
  }
  async fn scan_expired(&mut self) {
    let expired_ids: Vec<u32> = self
      .flows
      .iter()
      .filter_map(|(index, flow)| {
        if flow.expired.load(Ordering::Acquire)
          && !self.flows_info[*index as usize].as_ref().unwrap().is_multicast()
        {
          info!("removing expired flow (internal id {index}) dst {}", flow.remote);
          Some(*index)
        } else {
          None
        }
      })
      .collect_vec();
    for id in expired_ids {
      self.remove_flow_internal(id).await;
    }
  }
  /// The live unicast flows, for the next transmitter to restore. Multicast
  /// flows are restored by TransmitMulticasts from its own state, and
  /// expired flows have no receiver left to keep.
  pub fn snapshot(&self) -> Vec<SavedFlow> {
    self
      .flows
      .iter()
      .filter_map(|(&index, flow)| {
        let info = self.flows_info.get(index as usize)?.as_ref()?;
        if info.is_multicast() || flow.expired.load(Ordering::Acquire) {
          return None;
        }
        Some(SavedFlow {
          index,
          cookie: flow.cookie,
          remote: flow.remote,
          fpp: flow.fpp,
          bytes_per_sample: flow.bytes_per_sample,
          info: info.clone(),
        })
      })
      .collect()
  }

  /// Re-creates saved flows with their original index and cookie, so their
  /// receivers keep streaming (and their keepalives keep matching) across a
  /// transmitter restart. A flow whose layout no longer fits this
  /// transmitter, or whose slot or destination is already taken, is
  /// skipped. Returns how many were restored.
  pub async fn restore(&mut self, saved: Vec<SavedFlow>) -> usize {
    let mut restored = 0;
    for flow in saved {
      if flow.index >= MAX_FLOWS
        || self.flows.contains_key(&flow.index)
        || self.ip_port_to_id.contains_key(&flow.remote)
      {
        warn!("not restoring flow {} to {}: slot or destination in use", flow.index, flow.remote);
        continue;
      }
      if let Err(e) =
        validate_flow_layout(&flow.info.local_channel_indices, flow.fpp, flow.bytes_per_sample, self.num_channels)
      {
        warn!("not restoring flow {} to {}: {e}", flow.index, flow.remote);
        continue;
      }
      let socket = match UdpSocket::bind(SocketAddr::new(IpAddr::V4(self.self_info.ip_address), 0))
        .and_then(|sock| sock.connect(flow.remote).map(|_| sock))
        .and_then(|sock| sock.set_nonblocking(true).map(|_| sock))
      {
        Ok(sock) => sock,
        Err(e) => {
          warn!("not restoring flow {} to {}: {e}", flow.index, flow.remote);
          continue;
        }
      };
      let expired = Arc::new(AtomicBool::new(false));
      if self
        .commands_sender
        .send(Command::AddFlow {
          index: flow.index as usize,
          socket,
          channel_indices: flow.info.local_channel_indices.clone(),
          fpp: flow.fpp,
          bytes_per_sample: flow.bytes_per_sample,
          needs_keepalives: true,
          expired: expired.clone(),
        })
        .await
        .is_err()
      {
        break;
      }
      self.flows.insert(
        flow.index,
        FlowData {
          cookie: flow.cookie,
          remote: flow.remote,
          expired,
          fpp: flow.fpp,
          bytes_per_sample: flow.bytes_per_sample,
        },
      );
      self.ip_port_to_id.insert(flow.remote, flow.index);
      self.flows_info[flow.index as usize] = Some(flow.info);
      restored += 1;
    }
    restored
  }

  pub fn is_empty(&self) -> bool {
    self.flows.is_empty()
  }
  pub fn get_flows_info(&self) -> &Vec<Option<FlowInfo>> {
    &self.flows_info
  }
}

#[cfg(test)]
mod layout_tests {
  use super::*;

  #[test]
  fn accepts_typical_flows() {
    assert_eq!(validate_flow_layout(&[Some(0), Some(1)], 16, 3, 2), Ok(()));
    assert_eq!(validate_flow_layout(&[Some(7), None, Some(0)], 32, 4, 8), Ok(()));
    // multicast: 15 channels of 24-bit at 32 fpp still fit one packet
    assert_eq!(validate_flow_layout(&vec![Some(0); 15], 32, 3, 1), Ok(()));
  }

  #[test]
  fn rejects_what_would_panic_or_stall_the_tx_thread() {
    use FlowLayoutError::*;
    assert_eq!(validate_flow_layout(&[Some(0)], 16, 0, 1), Err(BytesPerSample(0)));
    assert_eq!(validate_flow_layout(&[Some(0)], 16, 536870911, 1), Err(BytesPerSample(536870911)));
    assert_eq!(validate_flow_layout(&[Some(0)], 0, 3, 1), Err(FramesPerPacket(0)));
    assert_eq!(validate_flow_layout(&[Some(0)], FPP_MAX as usize + 1, 3, 1), Err(FramesPerPacket(257)));
    assert_eq!(validate_flow_layout(&[Some(2)], 16, 3, 2), Err(ChannelIndex { index: 2, channels: 2 }));
    assert_eq!(
      validate_flow_layout(&[None, Some(usize::MAX)], 16, 3, 2),
      Err(ChannelIndex { index: usize::MAX, channels: 2 })
    );
    // 8 channels of 24-bit at FPP_MAX: within the old fpp check, but 6153 bytes
    assert_eq!(validate_flow_layout(&vec![Some(0); 8], 256, 3, 1), Err(PacketTooLarge(6153)));
    assert_eq!(validate_flow_layout(&vec![None; 700], 2, 4, 1), Err(PacketTooLarge(5609)));
  }

  #[test]
  fn empty_channel_list_is_ok() {
    assert_eq!(validate_flow_layout(&[], 2, 3, 1), Ok(()));
  }

  #[test]
  fn bytes_per_sample_out_of_range() {
    assert!(validate_flow_layout(&[Some(0)], 16, 1, 1).is_err());
    assert!(validate_flow_layout(&[Some(0)], 16, 5, 1).is_err());
  }

  #[test]
  fn channel_index_equal_to_count_rejected() {
    assert_eq!(
      validate_flow_layout(&[Some(2)], 16, 3, 2),
      Err(FlowLayoutError::ChannelIndex { index: 2, channels: 2 })
    );
  }

  #[test]
  fn packet_size_limit_is_exact() {
    // 9 + n * 4 * 2 <= MTU  =>  n = 186 fits, 187 does not
    assert_eq!(validate_flow_layout(&vec![None; 186], 2, 4, 1), Ok(()));
    assert!(validate_flow_layout(&vec![None; 187], 2, 4, 1).is_err());
  }
}

#[cfg(test)]
mod restart_tests {
  use super::*;
  use netdev::mac::MacAddr;

  fn device_info() -> DeviceInfo {
    DeviceInfo {
      ip_address: Ipv4Addr::LOCALHOST,
      netmask: Ipv4Addr::UNSPECIFIED,
      gateway: Ipv4Addr::UNSPECIFIED,
      mac_address: MacAddr::zero(),
      link_speed: 0,
      board_name: String::new(),
      manufacturer: String::new(),
      model_name: String::new(),
      model_number: String::new(),
      factory_device_id: [0; 8],
      process_id: 0,
      vendor_string: String::new(),
      friendly_hostname: String::new(),
      factory_hostname: String::new(),
      rx_channels: Vec::new(),
      tx_channels: Vec::new(),
      bits_per_sample: 24,
      pcm_type: 0,
      latency_ns: 10_000_000,
      sample_rate: 48000,
      arc_port: 0,
      cmc_port: 0,
      flows_control_port: 0,
      info_request_port: 0,
      product_version: None,
      name_request_path: None,
      latency_request_path: None,
      sample_rate_request_path: None,
      announced_sample_rate: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
      announced_latency_ns: Default::default(),
      state_dir: None,
    }
  }

  // A FlowsTransmitter without its TX thread: the receiver end of its
  // command channel stands in, so the commands it would send can be checked.
  fn transmitter(num_channels: usize) -> (FlowsTransmitter, mpsc::Receiver<Command>) {
    let (tx, rx) = mpsc::channel(32);
    (
      FlowsTransmitter {
        self_info: Arc::new(device_info()),
        flow_seq_id: 0.into(),
        flows: BTreeMap::new(),
        ip_port_to_id: BTreeMap::new(),
        commands_sender: tx,
        flows_info: (0..MAX_FLOWS).map(|_| None).collect(),
        num_channels,
      },
      rx,
    )
  }

  fn unicast(port: u16, channels: Vec<Option<usize>>) -> FlowInfo {
    FlowInfo {
      rx_hostname: Some("pi-b".into()),
      rx_flow_name: Some("f".into()),
      dst_addr: Ipv4Addr::LOCALHOST,
      dst_port: port,
      local_channel_indices: channels,
    }
  }

  #[tokio::test]
  async fn restart_keeps_unicast_flows_with_their_handles() {
    let (mut old, _old_rx) = transmitter(2);
    let (_, handle) = old.add_flow(unicast(41001, vec![Some(0), Some(1)]), 32, 3, None, false).await.unwrap();
    let (gone_idx, _) = old.add_flow(unicast(41002, vec![Some(0)]), 32, 3, None, false).await.unwrap();
    old.flows.get(&(gone_idx as u32)).unwrap().expired.store(true, Ordering::Release); // receiver gone
    let multicast = FlowInfo { rx_hostname: None, rx_flow_name: None, ..unicast(41003, vec![Some(1)]) };
    old.add_flow(multicast, 32, 3, None, true).await.unwrap();

    let saved = old.snapshot();
    assert_eq!(saved.len(), 1, "only the live unicast flow is kept: {saved:?}");

    let (mut new, mut new_rx) = transmitter(2);
    assert_eq!(new.restore(saved.clone()).await, 1);
    let index = u32::from_be_bytes(handle[0..4].try_into().unwrap());
    let cookie = u16::from_be_bytes(handle[4..6].try_into().unwrap());
    let flow = new.flows.get(&index).expect("flow restored at its old index");
    assert_eq!(flow.cookie, cookie, "same cookie, so the receiver's handle still matches");
    assert_eq!(new.ip_port_to_id.get(&"127.0.0.1:41001".parse().unwrap()), Some(&index));
    assert_eq!(new.flows_info[index as usize].as_ref().unwrap().local_channel_indices, vec![Some(0), Some(1)]);
    match new_rx.try_recv() {
      Ok(Command::AddFlow { index: i, needs_keepalives, fpp, bytes_per_sample, .. }) => {
        assert_eq!((i, needs_keepalives, fpp, bytes_per_sample), (index as usize, true, 32, 3));
      }
      other => panic!("expected AddFlow for the TX thread, got {other:?}"),
    }

    // restoring again finds the slot taken and does not duplicate it
    assert_eq!(new.restore(saved.clone()).await, 0);
    // a transmitter that no longer has channel 1 refuses the flow
    let (mut narrow, _rx) = transmitter(1);
    assert_eq!(narrow.restore(saved).await, 0);
    assert!(narrow.flows.is_empty());
  }
}

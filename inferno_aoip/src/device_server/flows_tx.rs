use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::num::Wrapping;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::{collections::BTreeMap, net::SocketAddr, sync::atomic::AtomicU32, time::Duration};

use atomic::Ordering;
use futures::FutureExt;
use itertools::Itertools;
use rand::rngs::SmallRng;
use rand::{thread_rng, Rng, SeedableRng};
use tokio::sync::watch;
use tokio::{select, sync::mpsc};

use super::samples_utils::*;
use super::tx_multicasts::MEDIA_PORT;
use crate::device_server::TransferNotifier;
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
  /// The actual ring buffer position last read from (start_ts from the most recent TX cycle).
  /// Exposed so external buffer writers can align their write positions.
  read_position: Arc<AtomicUsize>,
  /// Consistent (read_pos, monotonic_time) snapshot for precise timing calibration.
  read_position_snapshot: Option<Arc<super::ReadPositionSnapshot>>,
  /// Reference instant for monotonic_nanos in the snapshot.
  snapshot_ref_instant: std::time::Instant,
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
        self.read_position.store(start_ts, Ordering::Release);
        if let Some(snapshot) = &self.read_position_snapshot {
            let seq = snapshot.seq.load(Ordering::Relaxed);
            snapshot.seq.store(seq.wrapping_add(1), Ordering::Release); // odd = writing
            snapshot.read_position.store(start_ts, Ordering::Relaxed);
            let nanos = self.snapshot_ref_instant.elapsed().as_nanos() as u64;
            snapshot.monotonic_nanos.store(nanos, Ordering::Relaxed);
            snapshot.seq.store(seq.wrapping_add(2), Ordering::Release); // even = stable
        }
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
          info!("flow dst {:?} expired", flow.socket.peer_addr().ok());
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
    let mut next_process_events = now as Clock;
    drop(now);

    set_current_thread_realtime(81);
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
        self.on_transfer.as_ref().map(|transfer| (transfer.callback)());

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
}

#[derive(Debug)]
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

pub struct FlowsTransmitter {
  self_info: Arc<DeviceInfo>,
  flow_seq_id: AtomicU32,
  flows: BTreeMap<u32, FlowData>,
  ip_port_to_id: BTreeMap<SocketAddr, u32>,
  commands_sender: mpsc::Sender<Command>,
  flows_info: Vec<Option<FlowInfo>>,
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
    read_position: Arc<AtomicUsize>,
    read_position_snapshot: Option<Arc<super::ReadPositionSnapshot>>,
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
      read_position,
      read_position_snapshot,
      snapshot_ref_instant: std::time::Instant::now(),
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
    read_position: Arc<AtomicUsize>,
    read_position_snapshot: Option<Arc<super::ReadPositionSnapshot>>,
    on_transfer: Option<TransferNotifier>,
  ) -> (Self, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(100);
    let tx1 = tx.clone();
    let srate = self_info.sample_rate;
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
        read_position,
        read_position_snapshot,
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
    let dst_addr = SocketAddr::new(IpAddr::V4(flow_info.dst_addr), flow_info.dst_port);
    let (flow_index, cookie) = match self.ip_port_to_id.get(&dst_addr) {
      None => {
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
        self
          .commands_sender
          .send(Command::SetChannels {
            index: flow_index as usize,
            channel_indices: channel_indices.clone(),
          })
          .await
          .unwrap();
        (flow_index, self.flows.get(&flow_index).unwrap().cookie)
      }
    };

    self.ip_port_to_id.insert(dst_addr, flow_index);

    let mut flow_handle = [0u8; 6];
    flow_handle[0..4].copy_from_slice(&flow_index.to_be_bytes());
    flow_handle[4..6].copy_from_slice(&cookie.to_be_bytes());

    self.flows_info[flow_index as usize] = Some(flow_info);

    Ok((flow_index as usize, flow_handle))
  }
  pub fn activate_multicast_flow(&mut self, flow_index: u32) {
    // this is called for multicast flows after grace period
    self.flows.get(&flow_index).as_ref().unwrap().expired.store(false, Ordering::Release);
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
    if let Some((index, _)) = self.get_flow(handle) {
      let channel_indices = channel_indices.into_iter().collect_vec();
      self
        .commands_sender
        .send(Command::SetChannels { index: index as usize, channel_indices: channel_indices.clone() })
        .await
        .unwrap();

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
  pub fn is_empty(&self) -> bool {
    self.flows.is_empty()
  }
  pub fn get_flows_info(&self) -> &Vec<Option<FlowInfo>> {
    &self.flows_info
  }
}

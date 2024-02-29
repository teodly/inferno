use std::borrow::Borrow;
use std::f64::consts::PI;
use std::net::{IpAddr, SocketAddrV4, UdpSocket};
use std::num::Wrapping;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::{collections::BTreeMap, net::SocketAddr, sync::atomic::AtomicU32, time::Duration};

use atomic::Ordering;
use cirb::{wrapped_diff, Clock};
use futures::FutureExt;
use itertools::Itertools;
use rand::rngs::{SmallRng, ThreadRng};
use rand::{thread_rng, Rng, SeedableRng};
use tokio::sync::broadcast;
use tokio::{select, sync::mpsc};

use crate::os_utils::set_current_thread_realtime;
use crate::thread_utils::run_future_in_new_thread;
use crate::{common::*, DeviceInfo};
use crate::{media_clock::{ClockOverlay, MediaClock}, net_utils::MTU, protocol::flows_control::FlowHandle, Sample};
use crate::samples_utils::*;
use cirb::Output as RBOutput;
use cirb::Input as RBInput;

pub const FPP_MIN: u16 = 2;
pub const FPP_MAX: u16 = 32;
pub const MAX_FLOWS: u32 = 128;
pub const MAX_CHANNELS_IN_FLOW: u16 = 8;
pub const KEEPALIVE_TIMEOUT_SECONDS: usize = 4;
pub const MAX_LAG_SAMPLES: usize = 4800;
pub const DISCONTINUITY_THRESHOLD_SAMPLES: usize = 192000;
const BUFFERED_SAMPLES_PER_CHANNEL: usize = 65536;
pub const SELECT_THRESHOLD: Duration = Duration::from_millis(100);
pub const MIN_SLEEP: Duration = Duration::from_millis(2); // to save CPU cycles

pub type SamplesRequestCallback = Box<dyn FnMut(Clock, usize, &mut [Sample]) + Send + 'static>;

struct Flow {
  socket: UdpSocket,
  channel_indices: Vec<Option<usize>>,
  next_ts: Clock,
  fpp: usize,
  bytes_per_sample: usize,
  expires: Clock,
  expired: Arc<AtomicBool>,
}

impl Flow {
  fn bootstrap_next_ts(&mut self, now: Clock) {
    let remainder = now % self.fpp;
    self.next_ts = now.wrapping_add(self.fpp - remainder);
  }
  fn keep_alive(&mut self, now: Clock, sample_rate: u32) {
    self.expires = now.wrapping_add(KEEPALIVE_TIMEOUT_SECONDS * sample_rate as usize);
  }
}




#[derive(Debug)]
enum Command {
  NoOp,
  Shutdown,
  AddFlow { index: usize, socket: UdpSocket, channel_indices: Vec<Option<usize>>, fpp: usize, bytes_per_sample: usize, expired: Arc<AtomicBool> },
  RemoveFlow { index: usize },
  SetChannels { index: usize, channel_indices: Vec<Option<usize>> },
  UpdateClockOverlay(ClockOverlay),
}

struct FlowsTransmitterInternal {
  commands_receiver: mpsc::Receiver<Command>,
  sample_rate: u32,
  flows: Vec<Option<Flow>>,
  clock: MediaClock,
  channels_sources: Vec<RBOutput<Sample>>,
  send_latency_samples: usize,
  //callback: SamplesRequestCallback,
}

impl FlowsTransmitterInternal {
  fn now(&self) -> Option<Clock> {
    self.clock.now_in_timebase(self.sample_rate as u64)
  }

  #[inline(always)]
  fn transmit(&mut self, dither_rng: &mut SmallRng, process_events: bool) {
    let mut tmp_samples = [0 as Sample; FPP_MAX as usize];
    let mut pbuff = [0u8; MTU];
    let sample_rate = self.sample_rate;
    let max_lag_samples = MAX_LAG_SAMPLES;
    if let Some(now) = self.clock.now_in_timebase(sample_rate as u64) {
      let mut max_missing_samples = 0;
      for flow in &mut self.flows.iter_mut().filter_map(|opt|opt.as_mut()) {
        if flow.expired.load(Ordering::Relaxed) {
          continue;
        }
        let channels_in_flow = flow.channel_indices.len();
        let stride = channels_in_flow * flow.bytes_per_sample;
        let lag = wrapped_diff(now, flow.next_ts);
        if lag > max_lag_samples as isize {
          error!("tx lag of {} samples detected, or media clock jumped, dropout occurs!", lag);
          flow.bootstrap_next_ts(now);
        }
        if lag < -(DISCONTINUITY_THRESHOLD_SAMPLES as isize) {
          error!("media clock jumped: {}", lag);
          flow.bootstrap_next_ts(now);
        }
        pbuff[9..9 + stride * flow.fpp].fill(0);
        while wrapped_diff(now, flow.next_ts) >= 0 {
          pbuff[0] = 2u8; // ???
          let seconds = flow.next_ts / (sample_rate as Clock);
          let subsec_samples = flow.next_ts % (sample_rate as Clock);
          pbuff[1..5].copy_from_slice(&(seconds as u32).to_be_bytes());
          pbuff[5..9].copy_from_slice(&(subsec_samples as u32).to_be_bytes());
          let start_ts = flow.next_ts.wrapping_sub(flow.fpp).wrapping_sub(self.send_latency_samples);
          for (index_in_flow, &ch_opt) in flow.channel_indices.iter().enumerate() {
            if let Some(ch_index) = ch_opt {
              //(self.callback)(flow.next_ts, ch_index, &mut tmp_samples[0..flow.fpp]);
              let r = self.channels_sources[ch_index].read_at(start_ts, &mut tmp_samples[0..flow.fpp]);
              if r.useful_start_index != 0 || r.useful_end_index != flow.fpp {
                error!("didn't have enough samples, transmitting silence. {} {}", r.useful_start_index, flow.fpp-r.useful_end_index);

                tmp_samples[0..r.useful_start_index].fill(0);
                tmp_samples[r.useful_end_index..].fill(0);

                let missing_samples = r.useful_start_index + flow.fpp - r.useful_end_index;
                max_missing_samples = max_missing_samples.max(missing_samples);
              }
              let start = 9 + index_in_flow * flow.bytes_per_sample;
              let samples = &tmp_samples[0..flow.fpp];
              match flow.bytes_per_sample {
                2 => write_s16_samples(samples, &mut pbuff, start, stride, Some(dither_rng)),
                3 => write_s24_samples(samples, &mut pbuff, start, stride, Some(dither_rng)),
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
              flow.next_ts = flow.next_ts.wrapping_add(flow.fpp);
            } else {
              warn!("written {written}, should have {to_send}");
            }
          } else {
            warn!("send returned error");
          }
        }
        if process_events {
          if let Ok(_) = flow.socket.recv(&mut pbuff) {
            flow.keep_alive(now, sample_rate);
          } else if wrapped_diff(flow.expires, now) < 0 {
            flow.expired.store(true, Ordering::Release);
            info!("flow dst {:?} expired", flow.socket.peer_addr().ok());
          }
        }
      }
      /* if max_missing_samples != 0 {
        let prev = self.send_latency_samples;
        self.send_latency_samples = (self.send_latency_samples + max_missing_samples).min(4800);
        warn!("increasing transmit latency {} -> {}", prev, self.send_latency_samples);
      } */
    } else if self.flows.len() > 0 {
      error!("clock unavailable, can't transmit. is the PTP daemon running?");
    }
  }
  async fn run(&mut self) {
    let sample_rate = self.sample_rate;
    let mut dither_rng = SmallRng::from_rng(rand::thread_rng()).unwrap();
    let mut counter = Wrapping(0u32);
    set_current_thread_realtime(89);
    loop {
      let min_next_ts = self.flows.iter().filter_map(|opt|opt.as_ref()).map(|&ref flow|flow.next_ts).min_by(|&a, &b| wrapped_diff(a, b).cmp(&0));
      let sleep_duration = min_next_ts.and_then(|ts|self.clock.system_clock_duration_until(ts, sample_rate as u64)).unwrap_or(std::time::Duration::from_secs(20)).max(MIN_SLEEP);
      let mut process_events = (counter.0%16)==0;
      counter += 1;

      let command = if sleep_duration < SELECT_THRESHOLD {
        std::thread::sleep(sleep_duration);
        self.transmit(&mut dither_rng, process_events);
        if process_events {
          self.commands_receiver.try_recv().unwrap_or(Command::NoOp)
        } else {
          Command::NoOp
        }
      } else {
        process_events = true;
        select! {
          recv_opt = self.commands_receiver.recv() => {
            recv_opt.unwrap_or(Command::Shutdown)
          },
          _ = tokio::time::sleep(sleep_duration) => {
            self.transmit(&mut dither_rng, process_events);
            Command::NoOp
          }
        }
      };
      match command {
        Command::Shutdown => {
          break;
        },
        Command::AddFlow { index, socket, channel_indices, fpp, bytes_per_sample, expired } => {
          let mut flow = Flow { socket, channel_indices, next_ts: 0, fpp, bytes_per_sample, expires: 0, expired };
          if let Some(now) = self.now() {
            flow.bootstrap_next_ts(now);
            flow.keep_alive(now, self.sample_rate);
          }
          let previous = std::mem::replace(&mut self.flows[index], Some(flow));
          debug_assert!(previous.is_none());
        },
        Command::RemoveFlow { index } => {
          self.flows[index] = None; // TODO is freeing memory in realtime thread safe???
        },
        Command::SetChannels { index, channel_indices } => {
          self.flows[index].as_mut().unwrap().channel_indices = channel_indices;
        },
        Command::UpdateClockOverlay(clkovl) => {
          let had_clock = self.clock.is_ready();
          self.clock.update_overlay(clkovl);
          if !had_clock {
            let now = self.now().unwrap();
            for flow in &mut self.flows.iter_mut().filter_map(|opt|opt.as_mut()) {
              flow.bootstrap_next_ts(now);
              flow.keep_alive(now, self.sample_rate);
            }
          }
        },
        Command::NoOp => {}
      }
    }
  }
}


struct FlowInfo {
  cookie: u16,
  remote: SocketAddr,
  expired: Arc<AtomicBool>
}

pub struct FlowsTransmitter {
  self_info: Arc<DeviceInfo>,
  flow_seq_id: AtomicU32,
  flows: BTreeMap<u32, FlowInfo>,
  ip_port_to_id: BTreeMap<SocketAddr, u32>,
  commands_sender: mpsc::Sender<Command>
}

fn split_handle(h: FlowHandle) -> (u32, u16) {
  (u32::from_be_bytes(h[0..4].try_into().unwrap()), u16::from_be_bytes(h[4..6].try_into().unwrap()))
}

impl FlowsTransmitter {
  async fn run(rx: mpsc::Receiver<Command>, sample_rate: u32, latency_ns: usize, channels_outputs: Vec<RBOutput<Sample>>) {
    let mut internal = FlowsTransmitterInternal {
      commands_receiver: rx,
      sample_rate,
      flows: (0..MAX_FLOWS).map(|_|None).collect_vec(),
      clock: MediaClock::new(),
      channels_sources: channels_outputs,
      send_latency_samples: latency_ns * sample_rate as usize / 1_000_000_000,
      /* callback: Box::new(|mut timestamp: Clock, ch_index: usize, buffer: &mut [Sample]| {
        let period = 4 << ch_index;
        for sample in buffer {
          let angle = 2.0 * PI * (timestamp % period) as f64 / (period as f64);
          *sample = (angle.sin() * Sample::MAX as f64).round() as Sample;
          timestamp = timestamp.wrapping_add(1);
        }
      }) */
    };
    internal.run().await;
  }
  pub fn start(self_info: Arc<DeviceInfo>, mut media_clock_receiver: broadcast::Receiver<ClockOverlay>) -> (Self, Vec<RBInput<Sample>>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(100);
    let tx1 = tx.clone();
    tokio::spawn(async move {
      loop {
        let ovl_opt = media_clock_receiver.recv().await;
        match ovl_opt {
          Ok(overlay) => {
            tx1.send(Command::UpdateClockOverlay(overlay)).await.log_and_forget();
          },
          Err(broadcast::error::RecvError::Closed) => {
            break;
          },
          Err(e) => {
            warn!("clock receive error {e:?}");
          }
        }
      }
    });
    let srate = self_info.sample_rate;
    let (channels_inputs, channels_outputs) = (0..self_info.tx_channels.len()).map(|_| {
      cirb::RTHistory::<Sample>::new(BUFFERED_SAMPLES_PER_CHANNEL, 0).split()
    }).unzip();
    // TODO dehardcode latency_ns
    let thread_join = run_future_in_new_thread("flows TX", move || Self::run(rx, srate, 12_000_000, channels_outputs).boxed_local());
    return (Self {
      commands_sender: tx,
      self_info: self_info.clone(),
      flow_seq_id: 0.into(),
      flows: BTreeMap::new(),
      ip_port_to_id: BTreeMap::new()
    }, channels_inputs, thread_join);
  }
  pub async fn shutdown(&self) {
    self.commands_sender.send(Command::Shutdown).await.log_and_forget();
  }

  pub async fn add_flow(&mut self, dst_addr: SocketAddr, channel_indices: impl IntoIterator<Item=Option<usize>>, fpp: usize, bytes_per_sample: usize) -> Result<FlowHandle, std::io::Error> {
    let (flow_number, cookie) = match self.ip_port_to_id.get(&dst_addr) {
      None => {
        self.scan_expired().await;
        let mut counter = 0;
        let flow_number = loop {
          let flow_number = self.flow_seq_id.fetch_add(1, atomic::Ordering::AcqRel) % MAX_FLOWS;
          if !self.flows.contains_key(&flow_number) {
            break flow_number;
          }
          counter += 1;
          if counter > MAX_FLOWS {
            error!("ran out of flows! {MAX_FLOWS}");
            return Err(std::io::Error::from(std::io::ErrorKind::OutOfMemory));
          }
        };
        let flow = FlowInfo {
          cookie: thread_rng().gen(),
          remote: dst_addr.clone(),
          expired: Arc::new(AtomicBool::new(false))
        };
        
        let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(self.self_info.ip_address), 0))?;
        socket.connect(dst_addr)?;
        socket.set_nonblocking(true)?;
        
        self.commands_sender.send(Command::AddFlow { index: flow_number as usize, socket, channel_indices: channel_indices.into_iter().collect_vec(), fpp, bytes_per_sample, expired: flow.expired.clone() }).await.unwrap();

        let cookie = flow.cookie;
        self.flows.insert(flow_number, flow);
        (flow_number, cookie)
      },
      Some(&flow_number) => {
        warn!("got add flow request for already existing flow, setting channels instead");
        // TODO FIXME what if fpp or bytes_per_sample change?
        self.commands_sender.send(Command::SetChannels { index: flow_number as usize, channel_indices: channel_indices.into_iter().collect_vec() }).await.unwrap();
        (flow_number, self.flows.get(&flow_number).unwrap().cookie)
      }
    };

    self.ip_port_to_id.insert(dst_addr, flow_number);
    
    let mut flow_handle = [0u8; 6];
    flow_handle[0..4].copy_from_slice(&flow_number.to_be_bytes());
    flow_handle[4..6].copy_from_slice(&cookie.to_be_bytes());

    Ok(flow_handle)
  }
  fn get_flow(&self, handle: FlowHandle) -> Option<(u32, &FlowInfo)> {
    let (id, cookie) = split_handle(handle);
    self.flows.get(&id).filter(|flow| flow.cookie == cookie).map(|flow| (id, flow))
  }
  async fn remove_flow_internal(&mut self, id: u32) {
    if let Some(flow) = self.flows.remove(&id) {
      self.ip_port_to_id.remove(&flow.remote);
    }
    self.commands_sender.send(Command::RemoveFlow { index: id as usize }).await.unwrap();
  }
  pub async fn remove_flow(&mut self, handle: FlowHandle) -> bool {
    if let Some((id, _)) = self.get_flow(handle) {
      self.remove_flow_internal(id).await;
      true
    } else {
      false
    }
  }
  pub async fn set_channels(&mut self, handle: FlowHandle, channel_indices: impl IntoIterator<Item=Option<usize>>) -> bool {
    if let Some((id, _)) = self.get_flow(handle) {
      self.commands_sender.send(Command::SetChannels { index: id as usize, channel_indices: channel_indices.into_iter().collect_vec() }).await.unwrap();
      true
    } else {
      false
    }
  }
  async fn scan_expired(&mut self) {
    let expired_ids: Vec<u32> = self.flows.iter().filter_map(|(id, flow)| {
      if flow.expired.load(Ordering::Acquire) {
        info!("removing expired flow (internal id {id}) dst {}", flow.remote);
        Some(*id)
      } else {
        None
      }
    }).collect_vec();
    for id in expired_ids {
      self.remove_flow_internal(id).await;
    }
  }
}

use super::samples_utils::*;
use crate::device_info::DeviceInfo;
use crate::device_server::TransferNotifier;
use crate::net_utils::MTU;
use crate::ring_buffer::{ProxyToSamplesBuffer, RBInput, RingBufferShared};
use crate::util::os::set_current_thread_realtime;
use crate::util::real_time_box_channel::RealTimeBoxReceiver;
use crate::{common::*, media_clock::MediaClock};

use std::io::ErrorKind::WouldBlock;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicI32, AtomicUsize};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use atomic::Ordering;
use bool_vec::{boolvec, BoolVec};
use itertools::Itertools;
use mio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use usrvclock::ClockOverlay;

pub const MAX_FLOWS: usize = 32;
const WAKE_TOKEN: mio::Token = mio::Token(MAX_FLOWS);
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(250);
pub const CLOSING_SAMPLES_INTERVAL: Duration = Duration::from_millis(1);
const KEEPALIVE_CONTENT: [u8; 2] = [0x13, 0x37];

const SILENCE_BURST_LEN: usize = 64;
const SILENCE_SAMPLES: [Sample; SILENCE_BURST_LEN] = [0; SILENCE_BURST_LEN];

//pub type PacketCallback = Box<dyn FnMut(SocketAddr, &[u8]) + Send + 'static>;

struct Channel<P: ProxyToSamplesBuffer> {
  sinks: Vec<RBInput<Sample, P>>,
  timestamp_shift: ClockDiff,
  latency_samples: usize,
}

struct SocketData<P: ProxyToSamplesBuffer> {
  socket: UdpSocket,
  send_keepalives: bool,
  last_source: Option<SocketAddr>,
  last_packet_time: Arc<AtomicUsize>, // timebase: seconds since ref_instant
  bytes_per_sample: usize,
  latency_samples: usize,
  channels: Vec<Option<Channel<P>>>,
  empty_sinks_vecs: Vec<Vec<RBInput<Sample, P>>>,
  actual_latency_samples: Arc<AtomicI32>,
}

struct SilenceWriter<P: ProxyToSamplesBuffer> {
  sink: RBInput<Sample, P>,
  end_timestamp: Clock,
}

enum Command<P: ProxyToSamplesBuffer> {
  NoOp,
  Shutdown,
  AddSocket {
    index: usize,
    socket: SocketData<P>,
  },
  RemoveSocket {
    index: usize,
  },
  ConnectChannel {
    socket_index: usize,
    channel_in_flow: usize,
    sink: RBInput<Sample, P>,
  },
  DisconnectChannel {
    socket_index: usize,
    channel_in_flow: usize,
    rb_shared: Arc<RingBufferShared<Sample, P>>,
  },
}

struct FlowsReceiverInternal<P: ProxyToSamplesBuffer> {
  commands_receiver: mpsc::Receiver<Command<P>>,
  poll: mio::Poll,
  sockets: Vec<Option<SocketData<P>>>,
  silence_writers: Vec<SilenceWriter<P>>,
  sample_rate: u32,
  clock: MediaClock,
  clock_recv: RealTimeBoxReceiver<Option<ClockOverlay>>,
  ref_instant: Instant,
  on_transfer: Option<TransferNotifier>,
  current_timestamp: Arc<AtomicUsize>,
}

impl<P: ProxyToSamplesBuffer> FlowsReceiverInternal<P> {
  #[inline(always)]
  fn receive(
    sd: &mut SocketData<P>,
    sample_rate: u32,
    clock: &mut MediaClock,
    ref_instant: Instant,
    write: bool,
  ) -> Command<P> {
    let mut buf = [0; MTU];
    loop {
      match sd.socket.recv_from(&mut buf) {
        Ok((recv_size, src)) => {
          if recv_size < 9 {
            error!("received corrupted (too small) packet on flow socket");
            return Command::NoOp;
          }
          let timestamp = (u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize)
            .wrapping_mul(sample_rate as usize)
            .wrapping_add(u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) as usize)
            as Clock;
          // TODO: add timestamp sanity checks with PTP clock

          // TODO: optimize, fetching the hardware clock so often is suboptimal
          // fetch it every n packets or use timestamped_socket and kernel-level timestamps instead

          // can be wrapping because used only for latency calculation
          if let Some(now) = clock.wrapping_now_in_timebase(sample_rate.into()) {
            let latency = wrapped_diff(now, timestamp).clamp(0, i32::MAX as _);
            sd.actual_latency_samples.fetch_max(latency as _, Ordering::Relaxed);
          }

          if write {
            let num_channels = sd.channels.len();
            sd.last_packet_time.store(ref_instant.elapsed().as_secs() as _, Ordering::Relaxed);

            let _total_num_samples = (recv_size - 9) / sd.bytes_per_sample;
            //let audio_bytes = &buf[9..9+total_num_samples*sd.bytes_per_sample];
            let audio_bytes = &buf[9..recv_size];

            let stride = num_channels * sd.bytes_per_sample;
            let samples_count = audio_bytes.len() / stride;
            //info!("first byte = {}, assuming {} samples in {} channels", buf[0], samples_count, num_channels);
            for (i, ch) in sd.channels.iter_mut().enumerate() {
              if let Some(ch) = ch {
                let ts = timestamp.wrapping_add_signed(ch.timestamp_shift);
                ch.sinks.iter_mut().for_each(|sink| {
                  let reader = SamplesReader {
                    bytes: audio_bytes,
                    read_pos: i * sd.bytes_per_sample,
                    stride,
                    remaining_samples: samples_count,
                  };
                  match sd.bytes_per_sample {
                    2 => sink.write_from_at(ts as usize, S16ReaderIterator(reader)),
                    3 => sink.write_from_at(ts as usize, S24ReaderIterator(reader)),
                    4 => sink.write_from_at(ts as usize, S32ReaderIterator(reader)),
                    other => {
                      panic!("unsupported bytes per sample {}", other);
                    }
                  };
                });
              }
            }
            //(sd.callback)(src, &buf[..recv_size]);
          }
          sd.last_source = Some(src);
        }
        Err(e) => {
          if e.kind() != WouldBlock {
            error!("flow socket receive error: {:?}", e);
            // TODO recreate socket?
          }
          break;
        }
      }
    }
    return Command::NoOp;
  }
  async fn take_command(receiver: &mut mpsc::Receiver<Command<P>>) -> Command<P> {
    receiver.recv().await.unwrap_or(Command::Shutdown)
  }
  fn run(&mut self, mut start_time_rx: Option<tokio::sync::oneshot::Receiver<Clock>>) {
    let keepalive_interval_between_flows = KEEPALIVE_INTERVAL / self.sockets.len().try_into().unwrap();
    let mut next_keepalive = Instant::now() + keepalive_interval_between_flows;
    let mut keepalive_index = 0usize;
    let closing_samples_interval = self
      .on_transfer
      .as_ref()
      .map(|transfer| {
        Duration::from_nanos(
          // TODO: /2 is a HACK
          (transfer.max_interval_samples as u64 * 1_000_000_000u64 / self.sample_rate as u64) / 2,
        )
      })
      .unwrap_or(CLOSING_SAMPLES_INTERVAL);
    let mut next_closing_samples = Instant::now() + closing_samples_interval;
    let mut events = mio::Events::with_capacity(MAX_FLOWS + 1); // +1 because of waker, TODO: really necessary?
    let mut may_have_command = false;
    let mut start_timestamp = None;

    set_current_thread_realtime(80);
    loop {
      let write_to_rbs =
        self.sockets.iter().find(|opt| opt.is_some()).is_some() || self.silence_writers.len() > 0;
      let timeout = if may_have_command || write_to_rbs || self.on_transfer.is_some() {
        Some(closing_samples_interval)
      } else {
        self.current_timestamp.store(usize::MAX, Ordering::Release);
        None
      };
      self.poll.poll(&mut events, timeout).log_and_forget();

      if self.clock_recv.update() {
        if let Some(ovl) = self.clock_recv.get() {
          self.clock.update_overlay(*ovl);
        }
      }

      for event in &events {
        if event.token() == WAKE_TOKEN {
          may_have_command = true;
        } else {
          // received a packet from the network
          if let Some(rx) = &mut start_time_rx {
            // need to get start time to compute (ringbuffer_position - media_clock) difference
            // (because ALSA starts counting from 0)
            match rx.try_recv() {
              Ok(start_time) => {
                start_timestamp = Some(start_time);
                for socket_opt in &mut self.sockets {
                  if let Some(socket_data) = socket_opt {
                    for channel_opt in &mut socket_data.channels {
                      if let Some(channel) = channel_opt {
                        channel.timestamp_shift = (0 as ClockDiff)
                          .wrapping_sub_unsigned(start_time)
                          .wrapping_add_unsigned(channel.latency_samples.try_into().unwrap())
                          as ClockDiff;
                        // FIXME DRY
                      }
                    }
                  }
                }
              }
              Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
              Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                panic!("channel closed, unable to get start timestamp for ring buffer input");
              }
            }
          }
          if start_timestamp.is_some() {
            start_time_rx = None;
          }
          let socket_index = event.token().0;
          if let Some(socket_data) = &mut self.sockets[socket_index] {
            // always run receive to prevent network queue fill when waiting for start_time_rx
            // because network queue is harmful when working at realtime priority
            Self::receive(
              socket_data,
              self.sample_rate,
              &mut self.clock,
              self.ref_instant,
              start_time_rx.is_none(),
            );
          } else {
            warn!("got token not bound to any existing socket");
          }
        }
      }
      //if start_time_rx.is_none() {
      self.on_transfer.as_ref().map(|transfer| (transfer.callback)());
      //} XXX

      if may_have_command {
        match self.commands_receiver.try_recv() {
          Ok(command) => match command {
            Command::Shutdown => break,
            // MAYBE TODO: register/deregister appear to be thread safe so maybe they could be moved to non-real-time thread?
            Command::AddSocket { index, mut socket } => {
              debug!("adding socket");
              self
                .poll
                .registry()
                .register(&mut socket.socket, mio::Token(index), mio::Interest::READABLE)
                .unwrap();
              let previous = std::mem::replace(&mut self.sockets[index], Some(socket));
              debug_assert!(previous.is_none());
            }
            Command::RemoveSocket { index } => {
              self.poll.registry().deregister(&mut self.sockets[index].as_mut().unwrap().socket).unwrap();
              let socket = self.sockets[index].take().unwrap();
              if cfg!(debug_assertions) {
                let count: usize = socket
                  .channels
                  .iter()
                  .filter_map(|ch_opt| ch_opt.as_ref())
                  .map(|ch| ch.sinks.len())
                  .sum();
                if count > 0 {
                  error!(
                    "BUG: still have {} channels when removing socket index {index}",
                    socket.channels.len()
                  );
                }
              }
              let _ = socket;
            }
            Command::ConnectChannel { socket_index, channel_in_flow, sink } => {
              if cfg!(debug_assertions) {
                for sd in self.sockets.iter().filter_map(|opt| opt.as_ref()) {
                  for ch in sd.channels.iter().filter_map(|opt| opt.as_ref()) {
                    for existing_sink in &ch.sinks {
                      assert!(!Arc::ptr_eq(sink.shared(), existing_sink.shared()));
                    }
                  }
                }
              }
              let socket = self.sockets[socket_index].as_mut().unwrap();

              let timestamp_shift = (0 as ClockDiff)
                .wrapping_sub_unsigned(start_timestamp.unwrap_or(0))
                .wrapping_add_unsigned(socket.latency_samples.try_into().unwrap());

              // prefer existing sink - this ensures that buffer is erased properly but not excessively
              let sink = if let Some(existing_index) =
                self.silence_writers.iter().position(|sw| Arc::ptr_eq(sw.sink.shared(), sink.shared()))
              {
                self.silence_writers.swap_remove(existing_index).sink
                // TODO: previous sink is dropped. is freeing memory in realtime thread safe???
              } else {
                if let Some(now) = self.clock.wrapping_now_in_timebase(self.sample_rate.into()) {
                  sink.shared().reset(now.wrapping_add_signed(timestamp_shift));
                } else {
                  error!("clock not available, unable to reset ringbuffer when connecting channel");
                }
                sink
              };

              let channel_opt = &mut socket.channels[channel_in_flow];

              if channel_opt.is_none() {
                let mut sinks = socket.empty_sinks_vecs.pop().expect("ran out of empty sinks");
                debug_assert!(sinks.is_empty());
                debug_assert!(sinks.capacity() > 0);
                sinks.push(sink);
                *channel_opt =
                  Some(Channel { sinks, latency_samples: socket.latency_samples, timestamp_shift });
              } else {
                let channel = channel_opt.as_mut().unwrap();
                debug_assert!(channel.sinks.capacity() > channel.sinks.len());
                channel.sinks.push(sink);
              }
            }
            Command::DisconnectChannel { socket_index, channel_in_flow, rb_shared } => {
              if let Some(sd) = self.sockets[socket_index].as_mut() {
                if let Some(ch) = sd.channels[channel_in_flow].as_mut() {
                  let sink_index_opt =
                    ch.sinks.iter().position(|sink| Arc::ptr_eq(sink.shared(), &rb_shared));
                  if let Some(sink_index) = sink_index_opt {
                    let sink = ch.sinks.swap_remove(sink_index);
                    if let Some(now) = self.clock.wrapping_now_in_timebase(self.sample_rate.into()) {
                      let rb_size = sink.ring_buffer_size();
                      let writer = SilenceWriter {
                        sink,
                        end_timestamp: now
                          .wrapping_add(rb_size + rb_size / 2 /*TODO: ???*/)
                          .wrapping_add_signed(ch.timestamp_shift),
                      };
                      self.silence_writers.push(writer);
                    } else {
                      warn!("no media clock, unable to initialize SilenceWriter");
                    }
                  }
                }
              }
            }
            Command::NoOp => {}
          },
          Err(TryRecvError::Empty) => {
            may_have_command = false;
          }
          Err(TryRecvError::Disconnected) => {
            break;
          }
        };
      }

      let now = Instant::now();

      if start_time_rx.is_none() && now >= next_closing_samples {
        if let Some(now_ts) = self.clock.wrapping_now_in_timebase(self.sample_rate.into()) {
          let ts = now_ts.wrapping_sub(start_timestamp.unwrap_or(0));

          // Normally this position will be already written because in self.receive we are writing into future
          // (write position is increased by latency_samples)
          // However, if the stream breaks, it will ensure that buffer is filled with zeros
          for sd in self.sockets.iter_mut().filter_map(|opt| opt.as_mut()) {
            for ch in sd.channels.iter_mut().filter_map(|opt| opt.as_mut()) {
              for sink in &mut ch.sinks {
                sink.close_items_until(ts);
              }
            }
          }

          let mut finished = None;
          for (index, sw) in self.silence_writers.iter_mut().enumerate() {
            sw.sink.close_items_until(ts);
            if wrapped_diff(ts, sw.end_timestamp) >= 0 {
              finished = Some(index);
            }
          }
          //self.current_timestamp.store(if now_ts!=usize::MAX { now_ts } else { usize::MAX-1 }, Ordering::Release);

          if let Some(finished_index) = finished {
            self.silence_writers.swap_remove(finished_index);
          }
        } else {
          self.current_timestamp.store(usize::MAX, Ordering::Release);
        }

        next_closing_samples += closing_samples_interval;
        if next_closing_samples <= now {
          next_closing_samples = now + closing_samples_interval;
        }
      }

      if now >= next_keepalive {
        if let Some(sd) = &self.sockets[keepalive_index] {
          if sd.send_keepalives {
            if let Some(src) = sd.last_source {
              if let Err(e) = sd.socket.send_to(&KEEPALIVE_CONTENT, src) {
                error!("failed to send keepalive to {src:?}: {e:?}");
              } else {
                trace!("sent keepalive");
              }
            }
          }
        }
        next_keepalive += keepalive_interval_between_flows;
        if next_keepalive <= now {
          // nothing was received for very long, in that case don't spam with keepalives
          next_keepalive = now + keepalive_interval_between_flows;
        }
        keepalive_index += 1;
        if keepalive_index >= self.sockets.len() {
          keepalive_index = 0;
        }
      }
    }
  }
}

#[derive(Debug)]
pub struct FlowInfo {
  pub rx_port: u16,
  pub channels_map: Vec<BoolVec>,
  pub latency_samples: u32,
  pub actual_latency_samples: Arc<AtomicI32>,
}

pub struct FlowsReceiver<P: ProxyToSamplesBuffer> {
  commands_sender: mpsc::Sender<Command<P>>,
  waker: mio::Waker,
  max_channels: usize,
  pub flows_info: Arc<RwLock<Vec<Option<FlowInfo>>>>,
}

impl<P: ProxyToSamplesBuffer + Send + Sync + 'static> FlowsReceiver<P> {
  fn run(
    rx: mpsc::Receiver<Command<P>>,
    poll: mio::Poll,
    sample_rate: u32,
    ref_instant: Instant,
    clock_recv: RealTimeBoxReceiver<Option<ClockOverlay>>,
    start_time_rx: Option<tokio::sync::oneshot::Receiver<Clock>>,
    on_transfer: Option<TransferNotifier>,
    current_timestamp: Arc<AtomicUsize>,
    max_channels: usize,
  ) {
    let mut internal = FlowsReceiverInternal {
      commands_receiver: rx,
      sockets: (0..MAX_FLOWS).map(|_| None).collect_vec(),
      silence_writers: Vec::with_capacity(max_channels),
      poll,
      sample_rate,
      clock: MediaClock::new(false /* TODO */),
      clock_recv,
      ref_instant,
      on_transfer,
      current_timestamp,
    };
    internal.run(start_time_rx);
  }
  pub fn start(
    self_info: Arc<DeviceInfo>,
    clock_recv: RealTimeBoxReceiver<Option<ClockOverlay>>,
    ref_instant: Instant,
    start_time_rx: Option<tokio::sync::oneshot::Receiver<Clock>>,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
  ) -> (Self, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(100);
    let poll = mio::Poll::new().unwrap();
    let waker = mio::Waker::new(poll.registry(), WAKE_TOKEN).unwrap();
    let srate = self_info.sample_rate;
    let max_channels = self_info.rx_channels.len();
    let thread_join = std::thread::Builder::new()
      .name("flows RX".to_owned())
      .spawn(move || {
        Self::run(
          rx,
          poll,
          srate,
          ref_instant,
          clock_recv,
          start_time_rx,
          on_transfer,
          current_timestamp,
          max_channels,
        );
      })
      .unwrap();
    return (
      Self {
        commands_sender: tx,
        waker,
        max_channels,
        flows_info: Arc::new(RwLock::new((0..MAX_FLOWS).map(|_| None).collect_vec())),
      },
      thread_join,
    );
  }
  pub async fn shutdown(&self) {
    self.commands_sender.send(Command::Shutdown).await.log_and_forget();
    self.waker.wake().log_and_forget();
  }
  pub async fn add_socket(
    &self,
    local_index: usize,
    socket: UdpSocket,
    send_keepalives: bool,
    bytes_per_sample: usize,
    channels_count: usize,
    latency_samples: usize,
    last_packet_time_arc: Arc<AtomicUsize>,
  ) {
    // TODO: it would be more logical to move socket creation here from channels_subscriber.rs which is already convoluted
    debug!("adding flow receiver local index={local_index}");
    let empty_sinks_vecs = (0..channels_count)
      .map(|_| {
        let mut v = vec![];
        v.reserve_exact(self.max_channels);
        v
      })
      .collect_vec();
    let port = socket.local_addr().unwrap().port();
    let als: Arc<AtomicI32> = Arc::new(0.into());
    self.flows_info.write().unwrap()[local_index] = Some(FlowInfo {
      rx_port: port,
      channels_map: (0..channels_count).map(|_| boolvec![false; self.max_channels]).collect(),
      latency_samples: latency_samples.try_into().unwrap(),
      actual_latency_samples: als.clone(),
    });
    self
      .commands_sender
      .send(Command::AddSocket {
        index: local_index,
        socket: SocketData {
          socket,
          send_keepalives,
          last_source: None,
          last_packet_time: last_packet_time_arc,
          bytes_per_sample,
          latency_samples,
          channels: (0..channels_count).map(|_| None).collect(),
          empty_sinks_vecs,
          actual_latency_samples: als,
        },
      })
      .await
      .log_and_forget();
    self.waker.wake().log_and_forget();
  }
  pub async fn remove_socket(&self, local_index: usize) {
    debug!("removing flow receiver local index={local_index}");
    self.flows_info.write().unwrap()[local_index] = None;
    self.commands_sender.send(Command::RemoveSocket { index: local_index }).await.log_and_forget();
    self.waker.wake().log_and_forget();
  }
  pub async fn connect_channel(
    &self,
    local_flow_index: usize,
    channel_in_flow: usize,
    local_channel_index: usize,
    sink: RBInput<Sample, P>,
  ) {
    debug!("connecting channel: flow index={local_flow_index}, channel in flow: {channel_in_flow}");

    {
      let mut flows_info = self.flows_info.write().unwrap();
      flows_info[local_flow_index].as_mut().unwrap().channels_map[channel_in_flow]
        .set(local_channel_index, true);
    }

    self
      .commands_sender
      .send(Command::ConnectChannel { socket_index: local_flow_index, channel_in_flow, sink })
      .await
      .log_and_forget();
    self.waker.wake().log_and_forget();
  }
  pub async fn disconnect_channel(
    &self,
    local_flow_index: usize,
    channel_in_flow: usize,
    local_channel_index: usize,
    rb_shared: Arc<RingBufferShared<Sample, P>>,
  ) {
    debug!("disconnecting channel: flow index={local_flow_index}, channel in flow: {channel_in_flow}");

    {
      let mut flows_info = self.flows_info.write().unwrap();
      flows_info[local_flow_index].as_mut().unwrap().channels_map[channel_in_flow]
        .set(local_channel_index, false);
    }

    self
      .commands_sender
      .send(Command::DisconnectChannel { socket_index: local_flow_index, channel_in_flow, rb_shared })
      .await
      .log_and_forget();
    self.waker.wake().log_and_forget();
  }
}

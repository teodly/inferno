use crate::common::*;
use crate::device_info::DeviceInfo;
use crate::net_utils::MTU;
use crate::os_utils::set_current_thread_realtime;
use crate::samples_utils::*;
use crate::thread_utils::run_future_in_new_thread;

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use atomic::Ordering;
use cirb::Input as RBInput;
use futures::future::select_all;
use futures::{Future, FutureExt};
use itertools::Itertools;
use mio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

pub const MAX_FLOWS: usize = 128;
const WAKE_TOKEN: mio::Token = mio::Token(MAX_FLOWS);
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(250);
const KEEPALIVE_CONTENT: [u8; 2] = [0x13, 0x37];

//pub type PacketCallback = Box<dyn FnMut(SocketAddr, &[u8]) + Send + 'static>;

struct Channel {
  sink: RBInput<Sample>,
}

struct SocketData {
  socket: UdpSocket,
  last_source: Option<SocketAddr>,
  last_packet_time: Arc<AtomicUsize>,
  bytes_per_sample: usize,
  channels: Vec<Option<Channel>>,
}

enum Command {
  NoOp,
  Shutdown,
  AddSocket { index: usize, socket: SocketData },
  RemoveSocket { index: usize },
  ConnectChannel { socket_index: usize, channel_index: usize, sink: RBInput<Sample> },
  DisconnectChannel { socket_index: usize, channel_index: usize },
}

struct FlowsReceiverInternal {
  commands_receiver: mpsc::Receiver<Command>,
  poll: mio::Poll,
  sockets: Vec<Option<SocketData>>,
  sample_rate: u32,
  ref_instant: Instant,
}

impl FlowsReceiverInternal {
  fn receive(sd: &mut SocketData, sample_rate: u32, ref_instant: Instant) -> Command {
    let mut buf = [0; MTU];
    match sd.socket.recv_from(&mut buf) {
      Ok((recv_size, src)) => {
        if recv_size < 9 {
          error!("received corrupted (too small) packet on flow socket");
          return Command::NoOp;
        }

        let num_channels = sd.channels.len();
        sd.last_packet_time.store(ref_instant.elapsed().as_secs() as _, Ordering::Relaxed);

        let _total_num_samples = (recv_size - 9) / sd.bytes_per_sample;
        //let audio_bytes = &buf[9..9+total_num_samples*sd.bytes_per_sample];
        let audio_bytes = &buf[9..recv_size];
        let timestamp = (u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize)
          .wrapping_mul(sample_rate as usize)
          .wrapping_add(u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) as usize);

        // TODO: add timestamp sanity checks with PTP clock

        let stride = num_channels * sd.bytes_per_sample;
        let samples_count = audio_bytes.len() / stride;
        //info!("first byte = {}, assuming {} samples in {} channels", buf[0], samples_count, num_channels);
        for (i, ch) in sd.channels.iter_mut().enumerate() {
          if let Some(ch) = ch {
            let reader = SamplesReader {
              bytes: audio_bytes,
              read_pos: i * sd.bytes_per_sample,
              stride,
              remaining_samples: samples_count,
            };
            match sd.bytes_per_sample {
              2 => ch.sink.write_from_at(timestamp, S16ReaderIterator(reader)),
              3 => ch.sink.write_from_at(timestamp, S24ReaderIterator(reader)),
              4 => ch.sink.write_from_at(timestamp, S32ReaderIterator(reader)),
              other => {
                error!("BUG: unsupported bytes per sample {}", other);
                return Command::NoOp;
              }
            }
          }
        }
        //(sd.callback)(src, &buf[..recv_size]);
        sd.last_source = Some(src);
      }
      Err(e) => {
        error!("flow socket receive error: {:?}", e);
        // TODO recreate socket?
      }
    };
    return Command::NoOp;
  }
  async fn take_command(receiver: &mut mpsc::Receiver<Command>) -> Command {
    receiver.recv().await.unwrap_or(Command::Shutdown)
  }
  fn run(&mut self) {
    let mut next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;
    let mut events = mio::Events::with_capacity(MAX_FLOWS);
    let mut may_have_command = false;
    set_current_thread_realtime(88);
    loop {
      self.poll.poll(&mut events, if may_have_command { Some(Duration::from_millis(1)) } else { None }).log_and_forget();
      for event in &events {
        if event.token() == WAKE_TOKEN {
          may_have_command = true;
        } else {
          let socket_index = event.token().0;
          if let Some(socket_data) = &mut self.sockets[socket_index] {
            Self::receive(socket_data, self.sample_rate, self.ref_instant);
          } else {
            warn!("got token not bound to any existing socket");
          }
        }
      }
      if may_have_command {
        match self.commands_receiver.try_recv() {
          Ok(command) => match command {
            Command::Shutdown => break,
            Command::AddSocket { index: id, mut socket } => {
              self.poll.registry().register(&mut socket.socket, mio::Token(id), mio::Interest::READABLE).unwrap();
              let previous = std::mem::replace(&mut self.sockets[id], Some(socket));
              debug_assert!(previous.is_none());
            }
            Command::RemoveSocket { index: id } => {
              self.poll.registry().deregister(&mut self.sockets[id].as_mut().unwrap().socket).unwrap();
              self.sockets[id] = None;
            }
            Command::ConnectChannel { socket_index: socket_id, channel_index, sink } => {
              self.sockets[socket_id].as_mut().unwrap().channels[channel_index] = Some(Channel { sink });
            }
            Command::DisconnectChannel { socket_index: socket_id, channel_index } => {
              self.sockets[socket_id].as_mut().unwrap().channels[channel_index] = None;
            }
            Command::NoOp => {}
          }
          Err(TryRecvError::Empty) => {
            may_have_command = false;
          },
          Err(TryRecvError::Disconnected) => {
            break;
          }
        };
      }
      
      let now = Instant::now();
      if now >= next_keepalive {
        for sd in self.sockets.iter().filter_map(|opt|opt.as_ref()) {
          if let Some(src) = sd.last_source {
            if let Err(e) = sd.socket.send_to(&KEEPALIVE_CONTENT, src) {
              error!("failed to send keepalive to {src:?}: {e:?}");
            }
          }
        }
        next_keepalive += KEEPALIVE_INTERVAL;
        if next_keepalive <= now {
          // nothing was received for very long, in that case don't spam with keepalives
          next_keepalive = now + KEEPALIVE_INTERVAL;
        }
      }
    }
  }
}

pub struct FlowsReceiver {
  commands_sender: mpsc::Sender<Command>,
  waker: mio::Waker
}

impl FlowsReceiver {
  fn run(rx: mpsc::Receiver<Command>, poll: mio::Poll, sample_rate: u32, ref_instant: Instant) {
    let mut internal =
      FlowsReceiverInternal { commands_receiver: rx, sockets: (0..MAX_FLOWS).map(|_|None).collect_vec(), poll, sample_rate, ref_instant };
    internal.run();
  }
  pub fn start(self_info: Arc<DeviceInfo>, ref_instant: Instant) -> (Self, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(100);
    let poll = mio::Poll::new().unwrap();
    let waker = mio::Waker::new(poll.registry(), WAKE_TOKEN).unwrap();
    let srate = self_info.sample_rate;
    let thread_join = std::thread::Builder::new().name("flows RX".to_owned()).spawn(move || {
      Self::run(rx, poll, srate, ref_instant);
    }).unwrap();
    return (Self { commands_sender: tx, waker }, thread_join);
  }
  pub async fn shutdown(&self) {
    self.commands_sender.send(Command::Shutdown).await.log_and_forget();
  }
  pub async fn add_socket(
    &self,
    local_index: usize,
    socket: UdpSocket,
    bytes_per_sample: usize,
    channels_count: usize,
    last_packet_time_arc: Arc<AtomicUsize>,
  ) {
    debug!("adding flow receiver local index={local_index}");
    self
      .commands_sender
      .send(Command::AddSocket {
        index: local_index,
        socket: SocketData {
          socket,
          last_source: None,
          last_packet_time: last_packet_time_arc,
          bytes_per_sample,
          channels: (0..channels_count).map(|_| None).collect(),
        },
      })
      .await
      .log_and_forget();
    self.waker.wake().log_and_forget();
  }
  pub async fn remove_socket(&self, local_index: usize) {
    debug!("removing flow receiver local index={local_index}");
    self.commands_sender.send(Command::RemoveSocket { index: local_index }).await.log_and_forget();
    self.waker.wake().log_and_forget();
  }
  pub async fn connect_channel(&self, local_index: usize, channel_index: usize, sink: RBInput<Sample>) {
    debug!("connecting channel: flow index={local_index}, channel in flow: {channel_index}");
    self
      .commands_sender
      .send(Command::ConnectChannel { socket_index: local_index, channel_index, sink })
      .await
      .log_and_forget();
    self.waker.wake().log_and_forget();
  }
  pub async fn disconnect_channel(&self, local_index: usize, channel_index: usize) {
    debug!("disconnecting channel: flow index={local_index}, channel in flow: {channel_index}");
    self
      .commands_sender
      .send(Command::DisconnectChannel { socket_index: local_index, channel_index })
      .await
      .log_and_forget();
    self.waker.wake().log_and_forget();
  }
}

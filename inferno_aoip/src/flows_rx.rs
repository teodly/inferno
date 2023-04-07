use crate::common::*;
use crate::device_info::DeviceInfo;
use crate::net_utils::MTU;

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
use futures::Future;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

pub const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(250);
const KEEPALIVE_CONTENT: [u8; 2] = [0x13, 0x37];
//const KEEPALIVE_STRING: [u8; 9] = ['K' as u8, 'E' as u8, 'E' as u8, 'P' as u8, 'A' as u8, 'L' as u8, 'I' as u8, 'V' as u8, 'E' as u8];

//pub type PacketCallback = Box<dyn FnMut(SocketAddr, &[u8]) + Send + 'static>;

struct SamplesReader<'a> {
  bytes: &'a [u8],
  read_pos: usize,
  stride: usize,
  remaining_samples: usize,
}

impl<'a> SamplesReader<'a> {
  fn get_next_bytes(&mut self, count: usize) -> Option<&'a [u8]> {
    if self.read_pos + count > self.bytes.len() {
      return None;
    }
    let r = &self.bytes[self.read_pos..self.read_pos + count];
    self.read_pos += self.stride;
    self.remaining_samples -= 1;
    return Some(r);
  }
  fn size_hint(&self) -> (usize, Option<usize>) {
    let size = self.remaining_samples;
    (size, Some(size))
  }
}

struct S16ReaderIterator<'a>(SamplesReader<'a>);
struct S24ReaderIterator<'a>(SamplesReader<'a>);
struct S32ReaderIterator<'a>(SamplesReader<'a>);

impl<'a> Iterator for S16ReaderIterator<'a> {
  type Item = Sample;
  fn next(&mut self) -> Option<Sample> {
    self
      .0
      .get_next_bytes(2)
      .map(|b| (((b[0] as USample) << 24) | ((b[1] as USample) << 16)) as Sample)
  }
  fn size_hint(&self) -> (usize, Option<usize>) {
    self.0.size_hint()
  }
}
impl<'a> ExactSizeIterator for S16ReaderIterator<'a> {}

impl<'a> Iterator for S24ReaderIterator<'a> {
  type Item = Sample;
  fn next(&mut self) -> Option<Sample> {
    self.0.get_next_bytes(3).map(|b| {
      (((b[0] as USample) << 24) | ((b[1] as USample) << 16) | ((b[2] as USample) << 8)) as Sample
    })
  }
  fn size_hint(&self) -> (usize, Option<usize>) {
    self.0.size_hint()
  }
}
impl<'a> ExactSizeIterator for S24ReaderIterator<'a> {}

impl<'a> Iterator for S32ReaderIterator<'a> {
  type Item = Sample;
  fn next(&mut self) -> Option<Sample> {
    self.0.get_next_bytes(4).map(|b| {
      (((b[0] as USample) << 24)
        | ((b[1] as USample) << 16)
        | ((b[2] as USample) << 8)
        | (b[3] as USample)) as Sample
    })
  }
  fn size_hint(&self) -> (usize, Option<usize>) {
    self.0.size_hint()
  }
}
impl<'a> ExactSizeIterator for S32ReaderIterator<'a> {}

struct Channel {
  sink: RBInput<Sample>,
}

struct SocketData {
  socket: UdpSocket,
  last_source: Option<SocketAddr>,
  last_packet_time: Arc<AtomicUsize>,
  bytes_per_sample: usize,
  //callback: PacketCallback,
  channels: Vec<Option<Channel>>,
}

enum Command {
  NoOp,
  Shutdown,
  AddSocket { id: usize, socket: SocketData },
  RemoveSocket { id: usize },
  ConnectChannel { socket_id: usize, channel_index: usize, sink: RBInput<Sample> },
  DisconnectChannel { socket_id: usize, channel_index: usize },
}

struct FlowsReceiverInternal {
  commands_receiver: mpsc::Receiver<Command>,
  sockets: BTreeMap<usize, SocketData>,
  sample_rate: u32,
  ref_instant: Instant,
}

impl FlowsReceiverInternal {
  async fn receive(sd: &mut SocketData, sample_rate: u32, ref_instant: Instant) -> Command {
    let mut buf = [0; MTU];
    match sd.socket.recv_from(&mut buf).await {
      Ok((recv_size, src)) => {
        if recv_size < 9 {
          error!("received corrupted (too small) packet on flow socket");
          return Command::NoOp;
        }
        /*let num_channels = buf[0] as usize;
        if num_channels != sd.channels.len() {
          error!("received {num_channels} channels but this flow has {}", sd.channels.len());
          return Command::NoOp;
        }*/
        let num_channels = sd.channels.len();
        sd.last_packet_time.store(ref_instant.elapsed().as_secs() as _, Ordering::Relaxed);

        let _total_num_samples = (recv_size - 9) / sd.bytes_per_sample;
        //let audio_bytes = &buf[9..9+total_num_samples*sd.bytes_per_sample];
        let audio_bytes = &buf[9..recv_size];
        let timestamp = (u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize)
          .wrapping_mul(sample_rate as usize)
          .wrapping_add(u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]) as usize);

        // TODO: add timestamp sanity checks (when proper PTP clock receiver is implemented)

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
  async fn run(&mut self) {
    let mut next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;
    //let max_reordered_samples = (self.sample_rate/8) as usize;
    loop {
      // TODO check how (in)efficient is this... we have it called thousands of times per second... maybe migrate to mio crate
      // TODO OK, 4 flows taking 16% of core (--release build) on i5-2520M (2011), could be better
      let command_fut_arr = [Box::pin(Self::take_command(&mut self.commands_receiver))
        as Pin<Box<dyn Future<Output = Command>>>];
      let recv_iter = self.sockets.iter_mut().map(|(_id, sd)| {
        Box::pin(Self::receive(sd, self.sample_rate, self.ref_instant))
          as Pin<Box<dyn Future<Output = Command>>>
      });
      let (command, _, _) = select_all(recv_iter.chain(command_fut_arr)).await;
      match command {
        Command::Shutdown => break,
        Command::AddSocket { id, socket } => {
          self.sockets.insert(id, socket);
        }
        Command::RemoveSocket { id } => {
          self.sockets.remove(&id);
        }
        Command::ConnectChannel { socket_id, channel_index, sink } => {
          let sd = self.sockets.get_mut(&socket_id).unwrap();
          //assert!(sd.channels[channel_index].is_none());
          sd.channels[channel_index] = Some(Channel { sink });
        }
        Command::DisconnectChannel { socket_id, channel_index } => {
          let sd = self.sockets.get_mut(&socket_id).unwrap();
          sd.channels[channel_index] = None;
        }
        Command::NoOp => {}
      };
      let now = Instant::now();
      if now >= next_keepalive {
        for (_, sd) in &self.sockets {
          if let Some(src) = sd.last_source {
            if let Err(e) = sd.socket.send_to(&KEEPALIVE_CONTENT, src).await {
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
  commands_sender: mpsc::Sender<Command>
}

impl<'a> FlowsReceiver {
  async fn run(rx: mpsc::Receiver<Command>, sample_rate: u32, ref_instant: Instant) {
    let mut internal = FlowsReceiverInternal {
      commands_receiver: rx,
      sockets: BTreeMap::new(),
      sample_rate,
      ref_instant,
    };
    internal.run().await;
  }
  fn run_sync(rx: mpsc::Receiver<Command>, sample_rate: u32, ref_instant: Instant) {
    tokio::runtime::Builder::new_current_thread()
      .thread_name("media flows receiver")
      .enable_all()
      .build()
      .unwrap()
      .block_on(Self::run(rx, sample_rate, ref_instant));
  }
  pub fn start(self_info: Arc<DeviceInfo>, ref_instant: Instant) -> (Self, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(100);
    let srate = self_info.sample_rate;
    let thread_join = std::thread::spawn(move || Self::run_sync(rx, srate, ref_instant));
    return (Self { commands_sender: tx }, thread_join);
  }
  pub async fn shutdown(&self) {
    self.commands_sender.send(Command::Shutdown).await.log_and_forget();
  }
  pub async fn add_socket(
    &self,
    local_id: usize,
    socket: UdpSocket,
    bytes_per_sample: usize,
    channels_count: usize,
    last_packet_time_arc: Arc<AtomicUsize>,
  ) {
    self
      .commands_sender
      .send(Command::AddSocket {
        id: local_id,
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
  }
  pub async fn remove_socket(&self, local_id: usize) {
    self.commands_sender.send(Command::RemoveSocket { id: local_id }).await.log_and_forget();
  }
  pub async fn connect_channel(
    &self,
    local_id: usize,
    channel_index: usize,
    sink: RBInput<Sample>,
  ) {
    self
      .commands_sender
      .send(Command::ConnectChannel { socket_id: local_id, channel_index, sink })
      .await
      .log_and_forget();
  }
  pub async fn disconnect_channel(&self, local_id: usize, channel_index: usize) {
    self
      .commands_sender
      .send(Command::DisconnectChannel { socket_id: local_id, channel_index })
      .await
      .log_and_forget();
  }
}

pub async fn create_socket(self_ip: Ipv4Addr) -> tokio::io::Result<(UdpSocket, u16)> {
  let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(self_ip), 0)).await?;
  let port = socket.local_addr()?.port();
  return Ok((socket, port));
}

use std::{pin::Pin, sync::Arc, time::Duration};

use crate::{common::*, device_info::DeviceInfo, media_clock::{async_clock_receiver_to_realtime, ClockOverlay, MediaClock}, real_time_box_channel::{self, RealTimeBoxReceiver, RealTimeBoxSender}};
use cirb::{wrapped_diff, Clock, Output as RBOutput};
use futures::{Future, FutureExt};

use itertools::Itertools;
use tokio::{sync::{broadcast, mpsc}, time::interval};

const READ_INTERVAL: Duration = Duration::from_millis(50);
const BUFFER_SIZE: usize = 65536;
const SANE_CLOCK_DIFF: usize = 192000;
const MAX_LAG_SAMPLES: usize = 9600;

pub type SamplesCallback = Box<dyn FnMut(usize, &Vec<Vec<Sample>>) + Send + 'static>;


struct Channel {
  id: usize,
  source: RBOutput<Sample>,
  prev_holes_count: usize,
  latency_samples: Clock,
  was_connected: bool,
}

impl Channel {
  fn report_lost_samples(&self, timestamp: Clock, num_samples: usize, reason: &str) {
    error!("Lost {num_samples} samples at timestamp {timestamp} in channel id {} ({reason})", self.id);
  }
  fn read_samples_from_ringbuffer(&mut self, start_timestamp: Clock, buffer: &mut [Sample]) -> bool {
    let mut good = true;
    // report holes:
    let holes_count = self.source.holes_count();
    if holes_count != self.prev_holes_count {
      debug!("holes {} -> {}", self.prev_holes_count, holes_count);
      self.report_lost_samples(start_timestamp, buffer.len(), "reorder buffer timeout");
      self.prev_holes_count = holes_count;
      good = false;
    }

    // read samples:
    let r = self.source.read_at(start_timestamp, buffer);
    if r.useful_start_index != 0 {
      if self.was_connected {
        self.report_lost_samples(start_timestamp, r.useful_start_index, "buffer underrun or overwritten in the meantime");
        good = false;
      }
      // clear whatever junk data was contained at the beginning of buffer
      for sample in &mut buffer[0..r.useful_start_index] {
        *sample = 0;
      }
    }
    if r.useful_start_index < r.useful_end_index {
      self.was_connected = true;
    }
    if r.useful_end_index != buffer.len() {
      if self.was_connected {
        self.report_lost_samples( start_timestamp.wrapping_add(r.useful_end_index), buffer.len()-r.useful_end_index, "buffer underrun");
        good = false;
      }
      for sample in &mut buffer[r.useful_end_index..] {
        *sample = 0;
      }
    }
    if !good {
      warn!("wanted {start_timestamp}..{} but has ..{}", start_timestamp + buffer.len(), self.source.readable_until().unwrap_or(0));
    }
    good
  }
}

pub struct RealTimeSamplesReceiver {
  channels: Vec<RealTimeBoxReceiver<Option<Channel>>>,
  clock: MediaClock,
  clock_recv: RealTimeBoxReceiver<Option<ClockOverlay>>,
}

impl RealTimeSamplesReceiver {
  fn get_min_max_end_timestamps(&self) -> Option<(Clock, Clock)> {
    get_min_max_end_timestamps(self.channels.iter().map(|chrecv|chrecv.get()))
  }
  pub fn get_available_num_samples(&mut self, start_timestamp: Clock) -> usize {
    self.get_min_max_end_timestamps().map(|(end_ts, _)| {
      let diff = wrapped_diff(end_ts, start_timestamp);
      if diff > 0 {
        diff as Clock
      } else {
        0
      }
    }).unwrap_or(0)
  }
  pub fn get_samples(&mut self, start_timestamp: Clock, channel_index: usize, buffer: &mut [Sample]) -> bool {
    let chrecv = &mut self.channels[channel_index];
    chrecv.update();
    if let Some(ch) = chrecv.get_mut() {
      let start_timestamp = start_timestamp.wrapping_sub(ch.latency_samples).wrapping_sub(buffer.len());
      ch.read_samples_from_ringbuffer(start_timestamp, buffer)
    } else {
      buffer.fill(0);
      true
    }
  }
  pub fn clock(&mut self) -> &MediaClock {
    if self.clock_recv.update() {
      if let Some(ovl) = self.clock_recv.get() {
        self.clock.update_overlay(*ovl);
      }
    }
    &self.clock
  }
}


enum Command {
  NoOp,
  Shutdown,
  ConnectChannel { channel_index: usize, source: RBOutput<Sample>, latency_samples: usize },
  DisconnectChannel { channel_index: usize },
}

struct ToRealTime {
  commands_receiver: mpsc::Receiver<Command>,
  senders: Vec<RealTimeBoxSender<Option<Channel>>>,
}

impl ToRealTime {
  async fn run(&mut self) {
    loop {
      let command_opt = self.commands_receiver.recv().await;
      if !self.handle_command(command_opt).await {
        break;
      }
      for sender in &self.senders {
        sender.collect_garbage();
      }
    }
  }
  async fn handle_command(&mut self, command_opt: Option<Command>) -> bool {
    let command = command_opt.unwrap_or(Command::Shutdown);
    match command {
      Command::ConnectChannel{channel_index, source, latency_samples } => {
        debug!("connecting channel index={channel_index}");
        self.senders[channel_index].send(Box::new(Some(Channel {
          id: channel_index+1,
          source,
          prev_holes_count: 0,
          latency_samples,
          was_connected: false
        })));
      }
      Command::DisconnectChannel{channel_index} => {
        debug!("disconnecting channel index={channel_index}");
        self.senders[channel_index].send(Box::new(None));
      }
      Command::Shutdown => {
        return false;
      }
      Command::NoOp => {}
    };
    return true;
  }
}



struct PeriodicSamplesCollector {
  commands_receiver: mpsc::Receiver<Command>,
  channels: Vec<Option<Channel>>,
  callback: SamplesCallback,
}

fn get_min_max_end_timestamps<'a>(channels: impl IntoIterator<Item = &'a Option<Channel>>) -> Option<(Clock, Clock)> {
  let clocks = channels
    .into_iter()
    .filter_map(|opt| opt.as_ref())
    .map(|ch| ch.source.readable_until())
    .filter_map(|opt| opt)
    .collect_vec();
  Some((
    clocks.iter().min_by(|&&a, &&b| wrapped_diff(a, b).cmp(&0))?.to_owned(),
    clocks.iter().max_by(|&&a, &&b| wrapped_diff(a, b).cmp(&0))?.to_owned()
  ))
}

impl PeriodicSamplesCollector {
  fn get_min_max_end_timestamps(&self) -> Option<(Clock, Clock)> {
    get_min_max_end_timestamps(&self.channels)
  }
  async fn run(&mut self) {
    let mut clock = None;
    let mut read_data_interval = interval(READ_INTERVAL);
    read_data_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut channels_buffers = (0..self.channels.len()).map(|_| vec![0; BUFFER_SIZE]).collect_vec();
    loop {
      tokio::select! {
        _ = read_data_interval.tick() => {
          if let Some((min_end_ts, max_end_ts)) = self.get_min_max_end_timestamps() {
            // lag prevention logic to prevent abruptly disconnected transmitters from causing buffer overrun
            // in other audio channels
            let lag = max_end_ts.wrapping_sub(min_end_ts);
            let readable_until = if lag > MAX_LAG_SAMPLES {
              max_end_ts.wrapping_sub(MAX_LAG_SAMPLES)
            } else {
              min_end_ts
            };

            let new_clock = {
              if let Some(mut start_timestamp) = clock {
                // we already have clock, use it as a start timestamp

                let mut readable_samples_count = readable_until.wrapping_sub(start_timestamp);
                if readable_samples_count == 0 {
                  warn!("no new samples?!");
                } else {
                  //debug!("we have {readable_samples_count} new samples");
                }
                if readable_samples_count > SANE_CLOCK_DIFF {
                  error!("insane clock diff {readable_samples_count}, using {SANE_CLOCK_DIFF} last samples");
                  readable_samples_count = SANE_CLOCK_DIFF;
                  start_timestamp = readable_until.wrapping_sub(readable_samples_count);
                }
                if readable_samples_count > BUFFER_SIZE {
                  readable_samples_count = BUFFER_SIZE;
                }
                for chi in 0..self.channels.len() {
                  let mut buffer = channels_buffers[chi].as_mut_slice();
                  let ch_opt = {
                    if let Some(ch) = &mut self.channels[chi] {
                      // if ch.source.readable_until() changes from None to Some after get_*_timestamp() call,
                      // or lag prevention logic triggers,
                      // it would break our assumption that each channel has *at least* readable_samples_count readable.
                      // that's why we need this check.
                      match ch.source.readable_until() {
                        Some(ts) => if wrapped_diff(ts, readable_until) < 0 {
                          None
                        } else {
                          Some(ch)
                        }
                        None => None
                      }
                    } else {
                      None
                    }
                  };
                  if let Some(ch) = ch_opt {
                    ch.read_samples_from_ringbuffer(start_timestamp, &mut buffer[0..readable_samples_count]);
                  } else {
                    buffer[0..readable_samples_count].fill(0);
                  }
                }
                (self.callback)(readable_samples_count, &channels_buffers);
                start_timestamp.wrapping_add(readable_samples_count)
              } else {
                // we don't have clock yet, bootstrap it using currently available timestamp
                readable_until
              }
            };
            clock = Some(new_clock);
          }
        }
        command_opt = self.commands_receiver.recv() => {
          if !self.handle_command(command_opt).await {
            break;
          }
        }
      }
    }
  }
  async fn handle_command(&mut self, command_opt: Option<Command>) -> bool {
    let command = command_opt.unwrap_or(Command::Shutdown);
    match command {
      Command::ConnectChannel{channel_index, source, latency_samples } => {
        debug!("connecting channel index={channel_index}");
        self.channels[channel_index] = Some(Channel { id: channel_index+1, source, prev_holes_count: 0, latency_samples, was_connected: false });
      }
      Command::DisconnectChannel{channel_index} => {
        debug!("disconnecting channel index={channel_index}");
        self.channels[channel_index] = None;
      }
      Command::Shutdown => {
        return false;
      }
      Command::NoOp => {}
    };
    return true;
  }
}


pub struct SamplesCollector {
  commands_sender: mpsc::Sender<Command>,
}

impl SamplesCollector {

  pub fn new_with_callback(
    self_info: Arc<DeviceInfo>,
    callback: SamplesCallback,
  ) -> (Self, Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
    let (tx, rx) = mpsc::channel(100);
    let mut internal = PeriodicSamplesCollector {
      commands_receiver: rx,
      channels: (0..self_info.rx_channels.len()).map(|_| None).collect(),
      callback,
    };
    return (Self { commands_sender: tx }, async move { internal.run().await }.boxed());
  }

  pub fn new_realtime(self_info: Arc<DeviceInfo>, mut media_clock_receiver: broadcast::Receiver<ClockOverlay>) -> (Self, Pin<Box<dyn Future<Output = ()> + Send + 'static>>, RealTimeSamplesReceiver) {
    let (tx, rx) = mpsc::channel(100);
    let (senders, receivers) = (0..self_info.rx_channels.len()).map(|chi| {
      real_time_box_channel::channel(Box::new(None))
    }).unzip();

    let mut internal = ToRealTime {
      commands_receiver: rx,
      senders,
    };
    
    (Self {commands_sender: tx}, async move { internal.run().await }.boxed(), RealTimeSamplesReceiver {
      channels: receivers,
      clock: MediaClock::new(),
      clock_recv: async_clock_receiver_to_realtime(media_clock_receiver)
    })
  }

  pub async fn connect_channel(&self, channel_index: usize, source: RBOutput<Sample>, latency_samples: usize) {
    self
      .commands_sender
      .send(Command::ConnectChannel { channel_index, source, latency_samples })
      .await
      .log_and_forget();
  }
  pub async fn disconnect_channel(&self, channel_index: usize) {
    self.commands_sender.send(Command::DisconnectChannel { channel_index }).await.log_and_forget();
  }
  pub async fn shutdown(&self) {
    self.commands_sender.send(Command::Shutdown).await.log_and_forget();
  }
}

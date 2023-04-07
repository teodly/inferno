use std::{pin::Pin, sync::Arc, time::Duration};

use crate::{common::*, device_info::DeviceInfo};
use cirb::{wrapped_diff, Clock, Output as RBOutput};
use futures::{Future, FutureExt};

use itertools::Itertools;
use tokio::{sync::mpsc, time::interval};

const READ_INTERVAL: Duration = Duration::from_millis(50);
const BUFFER_SIZE: usize = 65536;
const SANE_CLOCK_DIFF: usize = 192000;
const MAX_LAG_SAMPLES: usize = 9600;

pub type SamplesCallback = Box<dyn FnMut(usize, &Vec<Vec<Sample>>) + Send + 'static>;

enum Command {
  NoOp,
  Shutdown,
  ConnectChannel { channel_index: usize, source: RBOutput<Sample> },
  DisconnectChannel { channel_index: usize },
}

struct Channel {
  source: RBOutput<Sample>,
  prev_holes_count: usize,
}

struct SamplesCollectorInternal {
  commands_receiver: mpsc::Receiver<Command>,
  channels: Vec<Option<Channel>>,
  channels_buffers: Vec<Vec<Sample>>,
  //next_ts: Option<Clock>
  callback: SamplesCallback,
}

impl SamplesCollectorInternal {
  fn get_end_timestamps(&self) -> impl Iterator<Item = Clock> + '_ {
    self
      .channels
      .iter()
      .filter_map(|opt| opt.as_ref())
      .map(|ch| ch.source.readable_until())
      .filter_map(|opt| opt)
  }
  fn get_min_max_end_timestamps(&self) -> Option<(Clock, Clock)> {
    let clocks = self.get_end_timestamps().collect_vec();
    if clocks.is_empty() {
      return None;
    }
    return Some((
      clocks.iter().min_by(|&a, &b| wrapped_diff(*a, *b).cmp(&0)).unwrap().to_owned(),
      clocks.iter().max_by(|&a, &b| wrapped_diff(*a, *b).cmp(&0)).unwrap().to_owned()
    ));
  }
  fn get_min_end_timestamp(&self) -> Option<Clock> {
    self.get_end_timestamps().min_by(|&a, &b| wrapped_diff(a, b).cmp(&0))
  }
  fn get_max_end_timestamp(&self) -> Option<Clock> {
    self.get_end_timestamps().max_by(|&a, &b| wrapped_diff(a, b).cmp(&0))
  }
  fn report_lost_samples(channel_index: usize, timestamp: Clock, num_samples: usize, reason: &str) {
    error!("Lost {num_samples} samples at timestamp {timestamp} in channel index={channel_index} ({reason})");
  }
  async fn run(&mut self) {
    let mut clock = None;
    let mut read_data_interval = interval(READ_INTERVAL);
    read_data_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut was_connected = vec![false; self.channels.len()];
    //let zeros: [Sample; BUFFER_SIZE] = [0; BUFFER_SIZE];
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
                  let mut buffer = self.channels_buffers[chi].as_mut_slice();
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
                    // report holes:
                    let holes_count = ch.source.holes_count();
                    if holes_count != ch.prev_holes_count {
                      debug!("holes {} -> {}", ch.prev_holes_count, holes_count);
                      Self::report_lost_samples(chi, start_timestamp, readable_samples_count, "reorder buffer timeout");
                      ch.prev_holes_count = holes_count;
                    }

                    // read samples:
                    let r = ch.source.read_at(start_timestamp, &mut buffer);
                    if r.useful_start_index != 0 {
                      if was_connected[chi] {
                        Self::report_lost_samples(chi, start_timestamp, r.useful_start_index, "buffer overrun");
                      }
                      // clear whatever junk data was contained at the beginning of buffer
                      for sample in &mut buffer[0..r.useful_start_index] {
                        *sample = 0;
                      }
                    }
                    was_connected[chi] = true;
                    assert!(r.useful_end_index >= readable_samples_count);
                  } else {
                    for sample in &mut buffer[0..readable_samples_count] {
                      *sample = 0;
                    }
                    was_connected[chi] = false;
                  }
                }
                (self.callback)(readable_samples_count, &self.channels_buffers);
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
          let command = command_opt.unwrap_or(Command::Shutdown);
          match command {
            Command::ConnectChannel{channel_index, source} => {
              debug!("connecting channel index={channel_index}");
              self.channels[channel_index] = Some(Channel { source, prev_holes_count: 0 });
            }
            Command::DisconnectChannel{channel_index} => {
              debug!("disconnecting channel index={channel_index}");
              self.channels[channel_index] = None;
            }
            Command::Shutdown => {
              break;
            }
            Command::NoOp => {}
          }
        }
      }
    }
  }
}

pub struct SamplesCollector {
  commands_sender: mpsc::Sender<Command>,
}

impl SamplesCollector {
  pub fn new(
    self_info: Arc<DeviceInfo>,
    callback: SamplesCallback,
  ) -> (Self, Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
    let (tx, rx) = mpsc::channel(100);
    let mut internal = SamplesCollectorInternal {
      commands_receiver: rx,
      channels: (0..self_info.rx_channels.len()).map(|_| None).collect(),
      channels_buffers: (0..self_info.rx_channels.len()).map(|_| vec![0; BUFFER_SIZE]).collect(),
      callback,
    };
    return (Self { commands_sender: tx }, async move { internal.run().await }.boxed());
  }
  pub async fn connect_channel(&self, channel_index: usize, source: RBOutput<Sample>) {
    self
      .commands_sender
      .send(Command::ConnectChannel { channel_index, source })
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

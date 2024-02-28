use std::time::Duration;

use clock_steering::unix::UnixClock;
use clock_steering::Clock as _;
use custom_error::custom_error;
use cirb::{Clock, ClockDiff};
use futures::AsyncWriteExt;
use interprocess::local_socket::tokio::LocalSocketStream;
use tokio::select;
use tokio::sync::broadcast;
use futures::io::AsyncReadExt;

use crate::{common::*, real_time_box_channel};
use crate::real_time_box_channel::RealTimeBoxReceiver;

#[derive(Clone, Copy, Debug)]
pub struct ClockOverlay {
  last_sync: Clock,
  shift: ClockDiff,
  freq_scale: f64,
}

custom_error! { pub ClockDecodeError
  BufferTooShort = "buffer too short",
  InvalidMagicNumber = "invalid magic number"
}

impl ClockOverlay {
  fn from_packet_buffer(buffer: &[u8]) -> Result<Self, ClockDecodeError> {
    let buf: [u8; 32] = buffer.try_into().map_err(|_|ClockDecodeError::BufferTooShort)?;
    if buf[0..8] != *b"TAIovl\x00\x01" {
      return Err(ClockDecodeError::InvalidMagicNumber);
    }
    Ok(Self {
      last_sync: Clock::from_ne_bytes(buf[8..16].try_into().unwrap()),
      shift: ClockDiff::from_ne_bytes(buf[16..24].try_into().unwrap()),
      freq_scale: f64::from_ne_bytes(buf[24..32].try_into().unwrap())
    })
  }
  fn timestamp_from_underlying(&self, ts: Clock) -> Clock {
    let elapsed = (ts as ClockDiff).wrapping_sub(self.last_sync as ClockDiff);
    let corr = (elapsed as f64 * self.freq_scale) as ClockDiff;
    (ts as ClockDiff).wrapping_add(self.shift).wrapping_add(corr) as Clock
  }
}

#[derive(Clone)]
pub struct MediaClock {
  clock: UnixClock,
  overlay: Option<ClockOverlay>,
}

#[inline(always)]
fn timestamp_to_clock_value(ts: clock_steering::Timestamp) -> Clock {
  (ts.seconds as Clock).wrapping_mul(1_000_000_000).wrapping_add(ts.nanos as Clock)
}

impl MediaClock {
  pub fn new() -> Self {
    Self {
      clock: UnixClock::CLOCK_TAI,
      overlay: None
    }
  }
  pub fn is_ready(&self) -> bool {
    self.overlay.is_some()
  }
  fn now_underlying(&self) -> Clock {
    timestamp_to_clock_value(self.clock.now().unwrap())
  }
  pub fn update_overlay(&mut self, overlay: ClockOverlay) {
    if let Some(cur_overlay) = self.overlay {
      let ro_now = self.now_underlying();
      let cur_ovl_time = cur_overlay.timestamp_from_underlying(ro_now);
      let new_ovl_time = overlay.timestamp_from_underlying(ro_now);
      let diff = (new_ovl_time as ClockDiff).wrapping_sub(cur_ovl_time as ClockDiff);
      if diff.abs() > 200_000_000 {
        error!("clock is trying to jump dangerously by {diff} ns, ignoring update");
        return;
      }
    }
    self.overlay = Some(overlay);
  }
  #[inline(always)]
  pub fn now_ns(&self) -> Option<Clock> {
    self.overlay.map(|overlay| {
      overlay.timestamp_from_underlying(self.now_underlying())
    })
  }
  #[inline(always)]
  pub fn now_in_timebase(&self, timebase_hz: u64) -> Option<Clock> {
    self.now_ns().map(|ns| {
      ((ns as u128) * (timebase_hz as u128) / 1_000_000_000u128) as Clock
    })
  }
  pub fn system_clock_duration_until(&self, timestamp: Clock, timebase_hz: u64) -> Option<std::time::Duration> {
    let now_ns = self.now_ns()?;
    let to_ns = (timestamp as u128 * 1_000_000_000u128 / timebase_hz as u128) as Clock;
    let remaining = (to_ns as ClockDiff).wrapping_sub(now_ns as ClockDiff);
    let corr = (remaining as f64 * self.overlay?.freq_scale) as ClockDiff;
    let duration = remaining - corr; // FIXME it should be * 1/(1+freq_scale) but should be good enough for low correction values
    if duration > 0 {
      Some(std::time::Duration::from_nanos(duration as u64))
    } else {
      Some(std::time::Duration::from_secs(0))
    }
  }
}


pub async fn start_clock_receiver(tx: broadcast::Sender<ClockOverlay>, mut shutdown: broadcast::Receiver<()>) {
  tokio::spawn(async move {
    let mut buff = [0u8; 32];
    async fn read_from_stream(stream_opt: &mut Option<LocalSocketStream>, buff: &mut [u8]) {
      loop {
        let stream = loop {
          if stream_opt.is_none() {
            let result = LocalSocketStream::connect("/tmp/ptp-clock-overlay").await;
            match result {
              Ok(stream) => {
                *stream_opt = Some(stream);
              },
              Err(e) => {
                warn!("could not open clock: {e:?}");
                tokio::time::sleep(Duration::from_secs(1)).await;
              }
            }
          }
          if let Some(stream) = stream_opt {
            break stream;
          }
        };
        match stream.read_exact(buff).await {
          Ok(_) => { break; },
          Err(e) => {
            warn!("could not read clock: {e:?}, will reopen stream");
            *stream_opt = None;
          }
        }
      }
    }
    let mut stream_opt = None;
    loop {
      select! {
        _ = read_from_stream(&mut stream_opt, &mut buff) => {
          match ClockOverlay::from_packet_buffer(&buff) {
            Ok(overlay) => {
              tx.send(overlay);
            },
            Err(e) => {
              warn!("failed to receive clock from PTP daemon: {e:?}");
            }
          }
        },
        _ = shutdown.recv() => {
          break;
        }
      }
      //break; // was testing whether the clock is to blame for tx buffer underruns
    }
    warn!("clock receiver exiting");
  });
}

pub fn async_clock_receiver_to_realtime(mut receiver: broadcast::Receiver<ClockOverlay>) -> RealTimeBoxReceiver<Option<ClockOverlay>> {
  let (rt_sender, rt_recv) = real_time_box_channel::channel(Box::new(None));
  tokio::spawn(async move {
    loop {
      let ovl_opt = receiver.recv().await;
      match ovl_opt {
        Ok(overlay) => {
          rt_sender.send(Box::new(Some(overlay)));
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
  rt_recv
}

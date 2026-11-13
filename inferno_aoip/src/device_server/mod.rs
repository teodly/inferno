use crate::mdns_client::{MdnsClient, PointerToMulticast};
use crate::media_clock::{
  async_clock_receiver_to_realtime, make_shared_media_clock, start_clock_receiver, ClockReceiver,
};
use crate::ring_buffer::{self, OwnedBuffer, ProxyToBuffer, ProxyToSamplesBuffer, RBOutput};
use crate::state_storage::StateStorage;
use atomic::Atomic;
use flows_tx::FlowsTransmitter;
use futures::{Future, FutureExt};
use itertools::Itertools;
use mdns_server::DeviceMDNSResponder;
use tokio::task::JoinHandle;
use tx_multicasts::TransmitMulticasts;

use std::collections::BTreeMap;
use std::io::Write;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, RwLock};

use std::time::{Duration, Instant};
use tokio::sync::{broadcast as broadcast_queue, mpsc, watch, Mutex};

use crate::common::*;
use crate::device_info::DeviceInfo;

/// Runs a request server and starts it again (after a growing pause) if it
/// panics. The servers parse packets from any host on the network; their
/// parsers are bounds-checked, and this keeps one unforeseen panic from taking
/// the service down until the process restarts. A panicked task drops its
/// socket while unwinding, so the restarted one can bind the same port.
async fn supervise<F, Fut>(name: &'static str, mut shutdown: broadcast_queue::Receiver<()>, start: F)
where
  F: Fn(broadcast_queue::Receiver<()>) -> Fut,
  Fut: Future<Output = ()> + Send + 'static,
{
  const FIRST_PAUSE: Duration = Duration::from_millis(100);
  const MAX_PAUSE: Duration = Duration::from_secs(5);
  const HEALTHY_RUN: Duration = Duration::from_secs(60);
  let mut pause = FIRST_PAUSE;
  loop {
    let started = Instant::now();
    match tokio::spawn(start(shutdown.resubscribe())).await {
      Ok(()) => return, // normal exit: shutdown
      Err(e) if e.is_panic() => {
        if started.elapsed() > HEALTHY_RUN {
          pause = FIRST_PAUSE;
        }
        error!("{name} panicked, restarting it in {pause:?}");
        tokio::select! {
          _ = shutdown.recv() => return,
          _ = tokio::time::sleep(pause) => {}
        }
        pause = (pause * 2).min(MAX_PAUSE);
      }
      Err(e) => {
        error!("{name} task ended unexpectedly: {e}");
        return;
      }
    }
  }
}

pub(crate) mod arc_server;
pub(crate) mod cmc_server;
pub(crate) mod flows_control_server;
pub(crate) mod info_mcast_server;
pub(crate) mod mdns_server;

pub(crate) mod channels_subscriber;
pub(crate) mod flows_rx;
pub(crate) mod flows_tx;
mod peaks;
pub(crate) mod metering;
pub(crate) mod samples_collector;
pub(crate) mod samples_utils;
pub(crate) mod saved_settings;
pub(crate) mod settings;
pub(crate) mod tx_multicasts;

pub use crate::common::{Clock, ClockDiff, Sample};
pub use crate::media_clock::{MediaClock, RealTimeClockReceiver};
pub use crate::ring_buffer::{new_owned, ExternalBufferParameters, PositionReportDestination, RBInput};
pub use settings::Settings;
pub type AtomicSample = atomic::Atomic<Sample>;

use channels_subscriber::{ChannelsBuffering, ChannelsSubscriber, ExternalBuffering, OwnedBuffering};
use peaks::peaks_of_buffers;
use samples_collector::{RealTimeSamplesReceiver, SamplesCallback, SamplesCollector};

const PEAKS_BUFFER_LEN: usize = 24000;
const PEAKS_ITER_SLEEP: Duration = Duration::from_millis(100);

pub struct TransferNotifier {
  pub callback: Box<dyn Fn() + Send + Sync>,
  pub max_interval_samples: Clock,
}

/// Rate limit for TransferNotifier callbacks from the realtime loops.
///
/// The RX and TX loops used to notify on every iteration, i.e. every packet
/// (thousands per second with a few flows). Each callback wakes the
/// application's blocked ALSA read or write, which finds less than a period
/// available and sleeps again: at 2 channels that churn was most of an idle
/// recorder's CPU. A quarter of max_interval_samples (the ALSA period)
/// between notifications still wakes the application well before each
/// period is complete.
pub(crate) struct NotifyThrottle {
  interval: Duration,
  next: Instant,
}

impl NotifyThrottle {
  pub(crate) fn new(max_interval_samples: Clock, sample_rate: u32) -> Self {
    let interval =
      Duration::from_nanos(max_interval_samples as u64 * 1_000_000_000 / 4 / (sample_rate.max(1) as u64));
    Self { interval, next: Instant::now() }
  }

  /// Whether a notification is due at `now`; if so, the next one is due an
  /// interval later.
  pub(crate) fn due(&mut self, now: Instant) -> bool {
    if now < self.next {
      return false;
    }
    self.next = now + self.interval;
    true
  }
}

#[cfg(test)]
mod notify_throttle_tests {
  use super::*;

  #[test]
  fn spaces_notifications_by_a_quarter_period() {
    let mut t = NotifyThrottle::new(1024, 48000); // 21.3ms period -> 5.33ms
    let t0 = Instant::now();
    t.next = t0;
    assert!(t.due(t0), "first call notifies");
    assert!(!t.due(t0 + Duration::from_millis(1)));
    assert!(!t.due(t0 + Duration::from_millis(5)));
    assert!(t.due(t0 + Duration::from_micros(5334)));
    // Spacing restarts from the notification, not from a fixed grid, so
    // a late loop iteration never produces a burst.
    assert!(t.due(t0 + Duration::from_millis(50)));
    assert!(!t.due(t0 + Duration::from_millis(51)));
    assert!(t.due(t0 + Duration::from_micros(55334)));
  }

  #[test]
  fn bounds_the_rate_of_a_per_packet_loop() {
    // 3000 iterations a second for one second notify under 190 times (the
    // quarter period rounds up to whole iterations), not 3000.
    let mut t = NotifyThrottle::new(1024, 48000);
    let t0 = Instant::now();
    t.next = t0;
    let n = (0..3000).filter(|i| t.due(t0 + Duration::from_micros(i * 333))).count();
    assert!((170..=190).contains(&n), "{n} notifications");
  }
}

pub struct DeviceServer {
  pub self_info: Arc<DeviceInfo>,
  ref_instant: Instant,
  state_storage: Arc<StateStorage>,
  clock_receiver: ClockReceiver,
  shared_media_clock: Arc<RwLock<MediaClock>>,
  mdns_client: Arc<MdnsClient>,
  mdns_server: Arc<DeviceMDNSResponder>,
  mcast_tx: mpsc::Sender<crate::protocol::mcast::MulticastMessage>,
  tx_latency_ns: u32,
  max_rx_latency_ns: u32,
  tx_source_bit_depth: u8,
  channels_sub_tx: watch::Sender<Option<Arc<ChannelsSubscriber>>>,
  channels_subscriber: Option<Arc<ChannelsSubscriber>>,
  flows_tx: Arc<Mutex<Option<FlowsTransmitter>>>,
  tx_multicasts: Arc<Mutex<Option<TransmitMulticasts>>>,
  tx_multicasts_by_channel: Arc<RwLock<BTreeMap<usize, PointerToMulticast>>>,
  rx_peaks_supplier: Arc<RwLock<Box<dyn Fn() -> Vec<u8> + Send + Sync>>>,
  tx_peaks_supplier: Arc<RwLock<Box<dyn Fn() -> Vec<u8> + Send + Sync>>>,
  shutdown_todo: Pin<Box<dyn Future<Output = ()> + Send>>,
  tx_shutdown_todo: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
  /// Unicast flows of the last stopped transmitter, restored by the next.
  saved_tx_flows: Arc<std::sync::Mutex<Vec<flows_tx::SavedFlow>>>,
  rx_shutdown_todo: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl DeviceServer {
  pub async fn start(settings: settings::Settings) -> Self {
    let self_info = Arc::new(settings.self_info);
    let state_storage = Arc::new(StateStorage::new(&self_info));
    let ref_instant = Instant::now();

    let (shutdown_send, shdn_recv1) = broadcast_queue::channel(16);
    let shdn_recv2 = shutdown_send.subscribe();
    let shdn_recv3 = shutdown_send.subscribe();
    let tx_multicasts_by_channel: Arc<RwLock<BTreeMap<usize, PointerToMulticast>>> = Default::default();
    let mdns_handle = Arc::new(mdns_server::DeviceMDNSResponder::start(
      self_info.clone(),
      settings.tx_latency_ns,
      tx_multicasts_by_channel.clone(),
    ));

    let mdns_client = Arc::new(crate::mdns_client::MdnsClient::new(self_info.clone()));
    let (mcast_tx, mcast_rx) = mpsc::channel(100);

    info!("clock path: {:?}", settings.clock_path);
    let clock_receiver = start_clock_receiver(settings.clock_path.clone());

    info!("waiting for clock");
    let shared_media_clock = make_shared_media_clock(&clock_receiver, settings.use_safe_clock).await;
    info!("clock ready");

    let mut tasks = vec![];

    let (channels_sub_tx, channels_sub_rx) = watch::channel(None);
    //let tx_peaks = Arc::new((0..self_info.tx_channels.len()).map(|_|AtomicSample::new(0)).collect_vec());
    //let rx_peaks = Arc::new((0..self_info.rx_channels.len()).map(|_|AtomicSample::new(0)).collect_vec());

    let rx_peaks_supplier: Arc<RwLock<Box<dyn Fn() -> Vec<u8> + Send + Sync>>> =
      Arc::new(RwLock::new(Box::new(|| -> Vec<u8> { vec![] })));
    let tx_peaks_supplier: Arc<RwLock<Box<dyn Fn() -> Vec<u8> + Send + Sync>>> =
      Arc::new(RwLock::new(Box::new(|| -> Vec<u8> { vec![] })));
    let txps = tx_peaks_supplier.clone();
    let rxps = rx_peaks_supplier.clone();
    let flows_tx: Arc<Mutex<Option<FlowsTransmitter>>> = Default::default();
    let tx_multicasts: Arc<Mutex<Option<TransmitMulticasts>>> = Default::default();
    tasks.append(&mut vec![
      tokio::spawn({
        let (self_info, state_storage, mdns_handle, mcast_tx, channels_sub_rx, flows_tx, tx_multicasts) = (
          self_info.clone(),
          state_storage.clone(),
          mdns_handle.clone(),
          mcast_tx.clone(),
          channels_sub_rx.clone(),
          flows_tx.clone(),
          tx_multicasts.clone(),
        );
        supervise("ARC server", shdn_recv1, move |shutdown| {
          arc_server::run_server(
            self_info.clone(),
            state_storage.clone(),
            mdns_handle.clone(),
            mcast_tx.clone(),
            channels_sub_rx.clone(),
            flows_tx.clone(),
            tx_multicasts.clone(),
            shutdown,
          )
        })
      }),
      tokio::spawn({
        let self_info = self_info.clone();
        supervise("CMC server", shdn_recv2, move |shutdown| {
          cmc_server::run_server(self_info.clone(), shutdown)
        })
      }),
      tokio::spawn(info_mcast_server::run_server(
        self_info.clone(),
        mcast_rx,
        shared_media_clock.clone(),
        channels_sub_rx,
        Box::new(move || ((rxps.read().unwrap())(), (txps.read().unwrap())())),
        shdn_recv3,
      )),
    ]);

    info!("all common tasks spawned");

    let mdns_server = mdns_handle.clone();
    let shutdown_todo = async move {
      shutdown_send.send(()).unwrap();
      for task in tasks {
        task.await.unwrap();
      }
      mdns_handle.shutdown_and_join();
    }
    .boxed();

    Self {
      self_info,
      ref_instant,
      state_storage,
      clock_receiver,
      shared_media_clock,
      mdns_client,
      mdns_server,
      mcast_tx,
      tx_latency_ns: settings.tx_latency_ns,
      max_rx_latency_ns: settings.max_rx_latency_ns,
      tx_source_bit_depth: settings.tx_source_bit_depth,
      channels_sub_tx,
      channels_subscriber: None,
      flows_tx,
      tx_multicasts,
      tx_multicasts_by_channel,
      rx_peaks_supplier,
      tx_peaks_supplier,
      //tasks,
      //tx_inputs,
      shutdown_todo,
      rx_shutdown_todo: None,
      tx_shutdown_todo: None,
      saved_tx_flows: Default::default(),
    }
  }

  pub async fn receive_with_callback(&mut self, samples_callback: SamplesCallback) {
    let (col, col_fut) = SamplesCollector::<OwnedBuffer<Atomic<Sample>>>::new_with_callback(
      self.self_info.clone(),
      Box::new(samples_callback),
    );
    let tasks = vec![tokio::spawn(col_fut)];
    let buffering = OwnedBuffering::new(524288 /*TODO*/, 4800 /*TODO*/, Arc::new(col));
    self.receive(tasks, None, buffering, Default::default(), None).await;
  }
  pub async fn receive_realtime(&mut self) -> RealTimeSamplesReceiver<OwnedBuffer<Atomic<Sample>>> {
    let (col, col_fut, rt_recv) =
      SamplesCollector::new_realtime(self.self_info.clone(), self.get_realtime_clock_receiver());
    let tasks = vec![tokio::spawn(col_fut)];
    let buffering = OwnedBuffering::new(524288 /*TODO*/, 4800 /*TODO*/, Arc::new(col));
    self.receive(tasks, None, buffering, Default::default(), None).await;

    rt_recv
  }
  pub async fn receive_to_external_buffer(
    &mut self,
    rx_channels_buffers: Vec<ExternalBufferParameters<Sample>>,
    start_time_rx: tokio::sync::oneshot::Receiver<Clock>,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
  ) {
    let buffering = ExternalBuffering::new(rx_channels_buffers, 4800 /*TODO*/);
    let rbs = buffering.ring_buffers.clone();
    *self.rx_peaks_supplier.write().unwrap() = Box::new(move || peaks_of_buffers(&rbs));
    self.receive(vec![], Some(start_time_rx), buffering, current_timestamp, on_transfer).await;
  }
  async fn receive<
    P: ProxyToSamplesBuffer + Send + Sync + 'static,
    B: ChannelsBuffering<P> + Send + Sync + 'static,
  >(
    &mut self,
    mut tasks: Vec<JoinHandle<()>>,
    start_time_rx: Option<tokio::sync::oneshot::Receiver<Clock>>,
    channels_buffering: B,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
  ) {
    let (srx1, srx2) = if let Some(in_rx) = start_time_rx {
      let (stx1, srx1) = tokio::sync::oneshot::channel::<Clock>();
      let (stx2, srx2) = tokio::sync::oneshot::channel::<Clock>();
      tokio::spawn(async {
        if let Ok(v) = in_rx.await {
          let _ = stx1.send(v);
          let _ = stx2.send(v);
        }
      });
      (Some(srx1), Some(srx2))
    } else {
      (None, None)
    };
    let (flows_rx_handle, flows_rx_thread) = flows_rx::FlowsReceiver::start(
      self.self_info.clone(),
      self.get_realtime_clock_receiver(),
      self.ref_instant,
      srx1,
      current_timestamp,
      on_transfer,
    );
    let flows_rx_handle = Arc::new(flows_rx_handle);
    let (channels_sub_handle, channels_sub_worker) = ChannelsSubscriber::new(
      self.self_info.clone(),
      self.shared_media_clock.clone(),
      flows_rx_handle.clone(),
      self.mdns_client.clone(),
      self.mcast_tx.clone(),
      channels_buffering,
      // The receiver's own minimum latency is RX_LATENCY_NS (it used the
      // transmit latency, so RX_LATENCY_NS was announced but never used).
      self.self_info.latency_ns.try_into().unwrap_or(u32::MAX),
      self.max_rx_latency_ns,
      self.state_storage.clone(),
      srx2,
      self.ref_instant,
    );
    let channels_sub_handle = Arc::new(channels_sub_handle);
    let _ = self.channels_sub_tx.send(Some(channels_sub_handle.clone()));
    self.channels_subscriber = Some(channels_sub_handle.clone());

    tasks.push(tokio::spawn(channels_sub_worker));

    let shutdown_todo = async move {
      flows_rx_handle.shutdown().await;
      channels_sub_handle.shutdown().await;
      flows_rx_thread.join().unwrap();
      for task in tasks {
        task.await.unwrap();
      }
      info!("receiver stopped");
    }
    .boxed();
    self.rx_shutdown_todo = Some(shutdown_todo);
  }
  pub async fn stop_receiver(&mut self) {
    let _ = self.channels_sub_tx.send(None);
    self.rx_shutdown_todo.take().unwrap().await;
  }

  pub async fn transmit_from_external_buffer(
    &mut self,
    tx_channels_buffers: Vec<ExternalBufferParameters<Sample>>,
    start_time_rx: tokio::sync::oneshot::Receiver<Clock>,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
  ) {
    let rb_outputs: Vec<_> =
      tx_channels_buffers.iter().map(|par| ring_buffer::wrap_external_source(par, 0)).collect();
    let rbs = rb_outputs.iter().map(|rbo| rbo.shared().clone()).collect_vec();
    *self.tx_peaks_supplier.write().unwrap() = Box::new(move || peaks_of_buffers(&rbs));
    self.transmit(Some(start_time_rx), rb_outputs, current_timestamp, on_transfer).await;
  }
  async fn transmit<P: ProxyToSamplesBuffer + Send + Sync + 'static>(
    &mut self,
    start_time_rx: Option<tokio::sync::oneshot::Receiver<Clock>>,
    rb_outputs: Vec<RBOutput<Sample, P>>,
    current_timestamp: Arc<AtomicUsize>,
    on_transfer: Option<TransferNotifier>,
  ) {
    let clock_rx = self.clock_receiver.subscribe();

    let (mut flows_tx_handle, flows_tx_thread) = flows_tx::FlowsTransmitter::start(
      self.self_info.clone(),
      self.tx_latency_ns.try_into().unwrap(),
      self.tx_source_bit_depth,
      self.get_realtime_clock_receiver(),
      rb_outputs.clone(),
      start_time_rx,
      current_timestamp.clone(),
      on_transfer,
    );
    // A transmitter restart (the ALSA plugin restarts it to recover from an
    // underrun) used to drop every flow: each receiver went silent until
    // its keepalive timed out and it re-requested, ~4-5 s later. Re-create
    // the previous transmitter's unicast flows with the same handles.
    let saved = std::mem::take(&mut *self.saved_tx_flows.lock().unwrap());
    if !saved.is_empty() {
      let restored = flows_tx_handle.restore(saved).await;
      info!("transmitter restarted with {restored} flow(s) kept");
    }
    *self.flows_tx.lock().await = Some(flows_tx_handle);
    let txm = TransmitMulticasts::new(
      self.tx_multicasts_by_channel.clone(),
      self.state_storage.clone(),
      self.self_info.clone(),
      self.flows_tx.clone(),
      self.mdns_server.clone(),
      self.mdns_client.clone(),
    );
    txm.load_state().await;
    *self.tx_multicasts.lock().await = Some(txm);
    for (index, _) in self.self_info.tx_channels.iter().enumerate() {
      self.mdns_server.remove_tx_channel(index);
      self.mdns_server.add_tx_channel(index);
    }
    let (shutdown_send, shutdown_recv) = broadcast_queue::channel(16);
    let flows_control_task = tokio::spawn({
      let (self_info, flows_tx) = (self.self_info.clone(), self.flows_tx.clone());
      supervise("flow control server", shutdown_recv, move |shutdown| {
        flows_control_server::run_server(self_info.clone(), flows_tx.clone(), shutdown)
      })
    });
    //let peaks_work = Arc::new(AtomicBool::new(true));
    //let peaks_work1 = peaks_work.clone();
    //let peaks = self.tx_peaks.clone();
    //let media_clock = self.shared_media_clock.clone();
    //let sample_rate = self.self_info.sample_rate;
    // TODO
    /* let peaks_thread = std::thread::Builder::new().name("peaks of TX".to_owned()).spawn(move || {
      let mut buf = vec![0 as Sample; PEAKS_BUFFER_LEN];
      while peaks_work.load(Ordering::Relaxed) {
        sleep(PEAKS_ITER_SLEEP);
        let read_from = 0; // TODO doesn't work because unconditional_read()==true in ALSA plugin
        for (chi, rbout) in rb_outputs.iter().enumerate() {
          let readable_until = rbout.readable_until();
          let to_read = readable_until.wrapping_sub(read_from).min(PEAKS_BUFFER_LEN);
          let start_timestamp = readable_until.wrapping_sub(to_read);
          rbout.read_at(start_timestamp, &mut buf);
          let samples = &buf[0..to_read];
          let peak = samples.iter().map(|n|n.saturating_abs()).max().unwrap_or(0);
          peaks[chi].fetch_max(peak, Ordering::Relaxed);
        }
      }
    }).unwrap(); */
    let flows_tx = self.flows_tx.clone();
    let tx_multicasts = self.tx_multicasts.clone();
    let saved_tx_flows = self.saved_tx_flows.clone();
    self.tx_shutdown_todo = Some(
      async move {
        //peaks_work1.store(false, Ordering::Relaxed);
        if let Some(txm) = tx_multicasts.lock().await.take() {
          txm.shutdown().await;
        }
        shutdown_send.send(()).unwrap();
        flows_control_task.await.unwrap();
        // keep the flows for the next transmitter (see transmit)
        if let Some(ftx) = flows_tx.lock().await.as_ref() {
          *saved_tx_flows.lock().unwrap() = ftx.snapshot();
        }
        *flows_tx.lock().await = None;
        flows_tx_thread.join().unwrap();
        //peaks_thread.join().unwrap();
        info!("transmitter stopped");
      }
      .boxed(),
    );
  }
  pub async fn stop_transmitter(&mut self) {
    self.tx_shutdown_todo.take().unwrap().await;
  }

  pub async fn subscribe(&self, local_channel_index: usize, tx_channel_name: &str, tx_hostname: &str) {
    if let Some(sub) = &self.channels_subscriber {
      sub.subscribe(local_channel_index, tx_channel_name, tx_hostname).await;
    } else {
      warn!("cannot subscribe: receiver not started");
    }
  }

  pub async fn unsubscribe(&self, local_channel_index: usize) {
    if let Some(sub) = &self.channels_subscriber {
      sub.unsubscribe(local_channel_index).await;
    } else {
      warn!("cannot unsubscribe: receiver not started");
    }
  }

  pub fn get_realtime_clock_receiver(&self) -> RealTimeClockReceiver {
    async_clock_receiver_to_realtime(
      self.clock_receiver.subscribe(),
      self.shared_media_clock.read().unwrap().get_overlay().clone(),
    )
  }

  pub async fn shutdown(self) {
    info!("shutting down");
    if let Some(todo) = self.rx_shutdown_todo {
      todo.await;
    }
    if let Some(todo) = self.tx_shutdown_todo {
      todo.await;
    }
    self.shutdown_todo.await;
    self.clock_receiver.stop().await.unwrap();
    info!("shutdown ok");
  }
}

#[cfg(test)]
mod supervise_tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};

  #[tokio::test]
  async fn restarts_a_panicking_server_and_stops_on_shutdown() {
    let (shutdown_send, shutdown_recv) = broadcast_queue::channel(4);
    let starts = Arc::new(AtomicUsize::new(0));
    let s = starts.clone();
    let supervisor = tokio::spawn(supervise("test server", shutdown_recv, move |mut shutdown| {
      let n = s.fetch_add(1, Ordering::SeqCst);
      async move {
        if n < 2 {
          panic!("simulated malformed packet");
        }
        let _ = shutdown.recv().await;
      }
    }));
    // two panics: pauses of 100 ms and 200 ms before the third start
    for _ in 0..100 {
      if starts.load(Ordering::SeqCst) >= 3 {
        break;
      }
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(starts.load(Ordering::SeqCst), 3);
    shutdown_send.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), supervisor).await.unwrap().unwrap();
    assert_eq!(starts.load(Ordering::SeqCst), 3, "not restarted after a normal exit");
  }

  #[tokio::test]
  async fn shutdown_during_restart_pause_stops_it() {
    let (shutdown_send, shutdown_recv) = broadcast_queue::channel(4);
    let supervisor = tokio::spawn(supervise("test server", shutdown_recv, |_shutdown| async {
      panic!("always");
    }));
    tokio::time::sleep(Duration::from_millis(30)).await;
    shutdown_send.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), supervisor).await.unwrap().unwrap();
  }
}

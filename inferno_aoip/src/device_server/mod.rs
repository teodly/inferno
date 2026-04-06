use crate::mdns_client::{MdnsClient, PointerToMulticast};
use crate::media_clock::{
  async_clock_receiver_to_realtime, make_shared_media_clock, start_clock_receiver, ClockReceiver,
};
use crate::ring_buffer::{self, ProxyToBuffer, ProxyToSamplesBuffer};
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

pub(crate) mod arc_server;
pub(crate) mod cmc_server;
pub(crate) mod flows_control_server;
pub(crate) mod info_mcast_server;
pub(crate) mod mdns_server;

pub(crate) mod channels_subscriber;
pub(crate) mod flows_rx;
pub(crate) mod flows_tx;
mod peaks;
pub(crate) mod samples_collector;
pub(crate) mod samples_utils;
pub(crate) mod saved_settings;
pub(crate) mod settings;
pub(crate) mod tx_multicasts;

pub use crate::common::{Clock, ClockDiff, Sample};
pub use crate::media_clock::{MediaClock, RealTimeClockReceiver};
pub use crate::ring_buffer::{ExternalBufferParameters, OwnedBuffer, PositionReportDestination, RBInput, RBOutput};
pub use crate::ring_buffer::new_owned as new_owned_ring_buffer;
pub use settings::Settings;
pub type AtomicSample = atomic::Atomic<Sample>;

/// Consistent (read_position, monotonic_time) snapshot written by the TX thread
/// at the exact point it updates `read_position`. Readers use a seqlock protocol:
/// odd seq = writer active, even seq = stable. Retry if seq changes between reads.
pub struct ReadPositionSnapshot {
    pub seq: AtomicUsize,
    pub read_position: AtomicUsize,
    /// Nanoseconds elapsed since a reference Instant stored in the TX thread.
    pub monotonic_nanos: std::sync::atomic::AtomicU64,
}

impl ReadPositionSnapshot {
    pub fn new() -> Self {
        Self {
            seq: AtomicUsize::new(0),
            read_position: AtomicUsize::new(usize::MAX),
            monotonic_nanos: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

use channels_subscriber::{ChannelsBuffering, ChannelsSubscriber, ExternalBuffering, OwnedBuffering};
use peaks::peaks_of_buffers;
use samples_collector::{RealTimeSamplesReceiver, SamplesCallback, SamplesCollector};

const PEAKS_BUFFER_LEN: usize = 24000;
const PEAKS_ITER_SLEEP: Duration = Duration::from_millis(100);

pub struct TransferNotifier {
  pub callback: Box<dyn Fn() + Send + Sync>,
  pub max_interval_samples: Clock,
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
  tx_source_bit_depth: u8,
  channels_sub_tx: watch::Sender<Option<Arc<ChannelsSubscriber>>>,
  flows_tx: Arc<Mutex<Option<FlowsTransmitter>>>,
  tx_multicasts: Arc<Mutex<Option<TransmitMulticasts>>>,
  tx_multicasts_by_channel: Arc<RwLock<BTreeMap<usize, PointerToMulticast>>>,
  rx_peaks_supplier: Arc<RwLock<Box<dyn Fn() -> Vec<u8> + Send + Sync>>>,
  tx_peaks_supplier: Arc<RwLock<Box<dyn Fn() -> Vec<u8> + Send + Sync>>>,
  shutdown_todo: Pin<Box<dyn Future<Output = ()> + Send>>,
  tx_shutdown_todo: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
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
      tokio::spawn(arc_server::run_server(
        self_info.clone(),
        state_storage.clone(),
        mdns_handle.clone(),
        mcast_tx.clone(),
        channels_sub_rx.clone(),
        flows_tx.clone(),
        tx_multicasts.clone(),
        shdn_recv1,
      )),
      tokio::spawn(cmc_server::run_server(self_info.clone(), shdn_recv2)),
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
      tx_source_bit_depth: settings.tx_source_bit_depth,
      channels_sub_tx,
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
      self.tx_latency_ns, /* FIXME should be RX latency */
      self.state_storage.clone(),
      srx2,
      self.ref_instant,
    );
    let channels_sub_handle = Arc::new(channels_sub_handle);
    let _ = self.channels_sub_tx.send(Some(channels_sub_handle.clone()));

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
    self.transmit(Some(start_time_rx), rb_outputs, current_timestamp, None, None, on_transfer).await;
  }

  /// Start transmitting from owned ring buffers.
  ///
  /// Creates `channel_count` owned ring buffers and returns the `RBInput` write handles.
  /// The caller writes audio samples via `RBInput::write_from_at()`.
  /// The `RBOutput` read handles are passed to the internal transmitter.
  ///
  /// Unlike `transmit_from_external_buffer`, owned buffers:
  /// - Track `readable_pos` on the write side (inferno only reads validated data)
  /// - Have `unconditional_read() == false` (reads check readable_pos)
  /// - Support hole detection and fill via `hole_fix_wait`
  ///
  /// The `read_position` atomic is updated by the FlowsTransmitter with the actual
  /// ring buffer position it reads from (`start_ts = next_ts + timestamp_shift`).
  /// This allows external writers to align their write positions correctly.
  pub async fn transmit_from_owned_buffer(
    &mut self,
    channel_count: usize,
    buffer_length: usize,
    hole_fix_wait: usize,
    start_time_rx: tokio::sync::oneshot::Receiver<Clock>,
    current_timestamp: Arc<AtomicUsize>,
    read_position: Arc<AtomicUsize>,
    read_position_snapshot: Option<Arc<ReadPositionSnapshot>>,
    on_transfer: Option<TransferNotifier>,
  ) -> Vec<ring_buffer::RBInput<Sample, OwnedBuffer<Atomic<Sample>>>> {
    let mut rb_inputs = Vec::with_capacity(channel_count);
    let mut rb_outputs = Vec::with_capacity(channel_count);
    for _ in 0..channel_count {
      let (input, output) = ring_buffer::new_owned(buffer_length, 0, hole_fix_wait);
      rb_inputs.push(input);
      rb_outputs.push(output);
    }
    let rbs = rb_outputs.iter().map(|rbo| rbo.shared().clone()).collect_vec();
    *self.tx_peaks_supplier.write().unwrap() = Box::new(move || peaks_of_buffers(&rbs));
    self.transmit(Some(start_time_rx), rb_outputs, current_timestamp, Some(read_position), read_position_snapshot, on_transfer).await;
    rb_inputs
  }

  async fn transmit<P: ProxyToSamplesBuffer + Send + Sync + 'static>(
    &mut self,
    start_time_rx: Option<tokio::sync::oneshot::Receiver<Clock>>,
    rb_outputs: Vec<RBOutput<Sample, P>>,
    current_timestamp: Arc<AtomicUsize>,
    read_position: Option<Arc<AtomicUsize>>,
    read_position_snapshot: Option<Arc<ReadPositionSnapshot>>,
    on_transfer: Option<TransferNotifier>,
  ) {
    let clock_rx = self.clock_receiver.subscribe();

    let (flows_tx_handle, flows_tx_thread) = flows_tx::FlowsTransmitter::start(
      self.self_info.clone(),
      self.tx_latency_ns.try_into().unwrap(),
      self.tx_source_bit_depth,
      self.get_realtime_clock_receiver(),
      rb_outputs.clone(),
      start_time_rx,
      current_timestamp.clone(),
      read_position.unwrap_or_else(|| Arc::new(AtomicUsize::new(usize::MAX))),
      read_position_snapshot,
      on_transfer,
    );
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
    let flows_control_task = tokio::spawn(flows_control_server::run_server(
      self.self_info.clone(),
      self.flows_tx.clone(),
      shutdown_recv,
    ));
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
    self.tx_shutdown_todo = Some(
      async move {
        //peaks_work1.store(false, Ordering::Relaxed);
        if let Some(txm) = tx_multicasts.lock().await.take() {
          txm.shutdown().await;
        }
        shutdown_send.send(()).unwrap();
        flows_control_task.await.unwrap();
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

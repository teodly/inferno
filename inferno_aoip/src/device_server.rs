use crate::channels_subscriber::{ChannelsBuffering, ChannelsSubscriber, ExternalBuffering, OwnedBuffering};
use crate::flows_tx::FlowsTransmitter;
use crate::info_mcast_server::MulticastMessage;
use crate::mdns_client::MdnsClient;
use crate::media_clock::{async_clock_receiver_to_realtime, make_shared_media_clock, start_clock_receiver, ClockReceiver};
use crate::protocol::flows_control;
use crate::real_time_box_channel::RealTimeBoxReceiver;
use crate::samples_collector::{RealTimeSamplesReceiver, SamplesCallback, SamplesCollector};
use crate::state_storage::StateStorage;
use crate::ring_buffer::{self, ExternalBuffer, ExternalBufferParameters, OwnedBuffer, ProxyToBuffer, ProxyToSamplesBuffer, RBInput, RBOutput};
use atomic::Atomic;
use futures::future::Join;
use futures::{Future, FutureExt};
use itertools::Itertools;
use tokio::sync::broadcast::Receiver;
use tokio::task::JoinHandle;
use usrvclock::ClockOverlay;

use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::mem::size_of;
use std::{env, os};
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, RwLock};

use std::net::IpAddr;
use std::time::Instant;
use tokio::sync::{broadcast as broadcast_queue, mpsc, watch};

use crate::device_info::{Channel, DeviceInfo};

use crate::{common::*, MediaClock, RealTimeClockReceiver};
use crate::protocol::proto_arc::PORT as ARC_PORT;
use crate::protocol::proto_cmc::PORT as CMC_PORT;
use crate::protocol::flows_control::PORT as FLOWS_CONTROL_PORT;
use crate::protocol::mcast::INFO_REQUEST_PORT as INFO_REQUEST_PORT;

pub trait SelfInfoBuilder {
  fn new_self(app_name: &str, short_app_name: &str, my_ip: Option<Ipv4Addr>) -> DeviceInfo;
  fn make_rx_channels(self, count: usize) -> DeviceInfo;
  fn make_tx_channels(self, count: usize) -> DeviceInfo;
}

impl SelfInfoBuilder for DeviceInfo {
  fn new_self(app_name: &str, short_app_name: &str, my_ip: Option<Ipv4Addr>) -> DeviceInfo {
    let my_ipv4 = my_ip.or_else(||
      env::var("INFERNO_BIND_IP").ok().map(|ipstr|
        ipstr.parse().expect("invalid IP in env var INFERNO_BIND_IP")
      )
    ).unwrap_or_else(||
      match local_ip_address::local_ip().expect("unknown local IP, cannot continue") {
        IpAddr::V4(a) => a,
        other => panic!("got local IP which is not IPv4: {other:?}"),
      }
    );
    let mut devid = [0u8; 8];
    env::var("INFERNO_DEVICE_ID").ok().map(|idstr| {
      hex::decode_to_slice(idstr, &mut devid).expect("invalid INFERNO_DEVICE_ID env var, should contain hex data");
    }).unwrap_or_else(|| {
      devid[2..6].copy_from_slice(&my_ipv4.octets());
    });

    // TODO make hostname and sample rate configurable from DC
    let friendly_hostname = env::var("INFERNO_NAME").ok().unwrap_or_else(||
      format!("{app_name} {}", hex::encode(&my_ipv4.octets()))
    );

    let sample_rate = env::var("INFERNO_SAMPLE_RATE").ok().
      map(|s|s.parse().expect("invalid INFERNO_SAMPLE_RATE, must be integer")).unwrap_or(48000);

    let mut result = DeviceInfo {
      ip_address: my_ipv4,
      board_name: "Inferno-AoIP".to_owned(),
      manufacturer: "Inferno-AoIP".to_owned(),
      model_name: app_name.to_owned(),
      factory_device_id: devid,
      process_id: 0,
      vendor_string: "Audinate Dante-compatible".to_owned(),
      factory_hostname: format!("{short_app_name}-{}", hex::encode(devid)),
      friendly_hostname,
      model_number: "_000000000000000b".to_owned(),
      rx_channels: vec![],
      tx_channels: vec![],
      bits_per_sample: 24, // TODO make it configurable
      pcm_type: 0xe,
      latency_ns: 10_000_000, // TODO make it configurable
      sample_rate,

      arc_port: ARC_PORT,
      cmc_port: CMC_PORT,
      flows_control_port: FLOWS_CONTROL_PORT,
      info_request_port: INFO_REQUEST_PORT,
    };

    if let Some(process_id) = env::var("INFERNO_PROCESS_ID").ok().map(|s|s.parse().expect("INFERNO_PROCESS_ID must be u16")) {
      result.process_id = process_id;
    }

    if let Some(altport) = env::var("INFERNO_ALT_PORT").ok().map(|s|s.parse().expect("INFERNO_ALT_PORT must be u16")) {
      result.arc_port = altport;
      result.cmc_port = altport+1;
      result.flows_control_port = altport+2;
      result.info_request_port = altport+3;
    }

    result
  }
  fn make_rx_channels(mut self, count: usize) -> DeviceInfo {
    self.rx_channels = (1..=count)
      .map(|id| Channel { factory_name: format!("{id:02}"), friendly_name: format!("RX {id}") })
      .collect_vec();
    self
  }
  fn make_tx_channels(mut self, count: usize) -> DeviceInfo {
    self.tx_channels = (1..=count)
      .map(|id| Channel { factory_name: format!("{id:02}"), friendly_name: format!("TX {id}") })
      .collect_vec();
    self
  }
}


pub struct DeviceServer {
  pub self_info: Arc<DeviceInfo>,
  ref_instant: Instant,
  state_storage: Arc<StateStorage>,
  clock_receiver: ClockReceiver,
  shared_media_clock: Arc<RwLock<MediaClock>>,
  mdns_client: Arc<MdnsClient>,
  mcast_tx: mpsc::Sender<MulticastMessage>,
  channels_sub_tx: watch::Sender<Option<Arc<ChannelsSubscriber>>>,
  //tx_inputs: Vec<RBInput<Sample, P>>,
  //tasks: Vec<JoinHandle<()>>,
  shutdown_todo: Pin<Box<dyn Future<Output = ()> + Send>>,
  tx_shutdown_todo: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
  rx_shutdown_todo: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl DeviceServer {
  pub async fn start(self_info: DeviceInfo) -> Self {
    let self_info = Arc::new(self_info);
    let state_storage = Arc::new(StateStorage::new(&self_info));
    let ref_instant = Instant::now();

    let (shutdown_send, shdn_recv1) = broadcast_queue::channel(16);
    let shdn_recv2 = shutdown_send.subscribe();
    let shdn_recv3 = shutdown_send.subscribe();
    let mdns_handle = crate::mdns_server::start_server(self_info.clone());

    let mdns_client = Arc::new(crate::mdns_client::MdnsClient::new(self_info.ip_address));
    let (mcast_tx, mcast_rx) = mpsc::channel(100);

    let clock_receiver = start_clock_receiver();

    info!("waiting for clock");
    // MAYBE TODO refactor to tokio::sync::watch
    let shared_media_clock = make_shared_media_clock(&clock_receiver).await;
    info!("clock ready");

    let mut tasks = vec![];

    let (channels_sub_tx, channels_sub_rx) = watch::channel(None);

    tasks.append(&mut vec![
      tokio::spawn(crate::arc_server::run_server(
        self_info.clone(),
        channels_sub_rx,
        shdn_recv1,
      )),
      tokio::spawn(crate::cmc_server::run_server(self_info.clone(), shdn_recv2)),
      tokio::spawn(crate::info_mcast_server::run_server(self_info.clone(), mcast_rx, shdn_recv3)),
    ]);

    info!("all common tasks spawned");

    let shutdown_todo = async move {
      shutdown_send.send(()).unwrap();
      mdns_handle.shutdown().unwrap();
      for task in tasks {
        task.await.unwrap();
      }
    }.boxed();

    Self {
      self_info,
      ref_instant,
      state_storage,
      clock_receiver,
      shared_media_clock,
      mdns_client,
      mcast_tx,
      channels_sub_tx,
      //tasks,
      //tx_inputs,
      shutdown_todo,
      rx_shutdown_todo: None,
      tx_shutdown_todo: None,
    }
  }

  pub async fn receive_with_callback(&mut self, samples_callback: SamplesCallback) {
    let (col, col_fut) = SamplesCollector::<OwnedBuffer<Atomic<Sample>>>::new_with_callback(self.self_info.clone(), Box::new(samples_callback));
    let tasks = vec![tokio::spawn(col_fut)];
    let buffering = OwnedBuffering::new(524288 /*TODO*/, 4800 /*TODO*/, Arc::new(col));
    self.receive(tasks, None, buffering, Default::default(), None).await;
  }
  pub async fn receive_realtime(&mut self) -> RealTimeSamplesReceiver<OwnedBuffer<Atomic<Sample>>> {
    let (col, col_fut, rt_recv) = SamplesCollector::new_realtime(self.self_info.clone(), self.get_realtime_clock_receiver());
    let tasks = vec![tokio::spawn(col_fut)];
    let buffering = OwnedBuffering::new(524288 /*TODO*/, 4800 /*TODO*/, Arc::new(col));
    self.receive(tasks, None, buffering, Default::default(), None).await;

    rt_recv
  }
  pub async fn receive_to_external_buffer(&mut self, rx_channels_buffers: Vec<ExternalBufferParameters<Sample>>, start_time_rx: tokio::sync::oneshot::Receiver<Clock>, current_timestamp: Arc<AtomicUsize>, on_transfer: Box<dyn Fn() + Send>) {
    let buffering = ExternalBuffering::new(rx_channels_buffers, 4800 /*TODO*/);
    self.receive(vec![], Some(start_time_rx), buffering, current_timestamp, Some(on_transfer)).await;
  }
  async fn receive<P: ProxyToSamplesBuffer + Send + Sync + 'static, B: ChannelsBuffering<P> + Send + Sync + 'static>(&mut self, mut tasks: Vec<JoinHandle<()>>, start_time_rx: Option<tokio::sync::oneshot::Receiver<Clock>>, channels_buffering: B, current_timestamp: Arc<AtomicUsize>, on_transfer: Option<Box<dyn Fn() + Send>>) {
    let (flows_rx_handle, flows_rx_thread) = crate::flows_rx::FlowsReceiver::start(self.self_info.clone(), self.get_realtime_clock_receiver(), self.ref_instant, start_time_rx, current_timestamp, on_transfer);
    let flows_rx_handle = Arc::new(flows_rx_handle);
    let (channels_sub_handle, channels_sub_worker) = ChannelsSubscriber::new(
      self.self_info.clone(),
      self.shared_media_clock.clone(),
      flows_rx_handle.clone(),
      self.mdns_client.clone(),
      self.mcast_tx.clone(),
      channels_buffering,
      self.state_storage.clone(),
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
    }.boxed();
    self.rx_shutdown_todo = Some(shutdown_todo);
  }
  pub async fn stop_receiver(&mut self) {
    let _ = self.channels_sub_tx.send(None);
    self.rx_shutdown_todo.take().unwrap().await;
  }

  pub async fn transmit_from_external_buffer(&mut self, tx_channels_buffers: Vec<ExternalBufferParameters<Sample>>, start_time_rx: tokio::sync::oneshot::Receiver<Clock>, current_timestamp: Arc<AtomicUsize>, on_transfer: Box<dyn Fn() + Send>) {
    let rb_outputs = tx_channels_buffers.iter().map(|par| ring_buffer::wrap_external_source(par, 0)).collect();
    self.transmit(Some(start_time_rx), rb_outputs, current_timestamp, Some(on_transfer)).await;
  }
  async fn transmit<P: ProxyToSamplesBuffer + Send + Sync + 'static>(&mut self, start_time_rx: Option<tokio::sync::oneshot::Receiver<Clock>>, rb_outputs: Vec<RBOutput<Sample, P>>, current_timestamp: Arc<AtomicUsize>, on_transfer: Option<Box<dyn Fn() + Send>>) {
    let clock_rx = self.clock_receiver.subscribe();
    
    let (flows_tx_handle, flows_tx_thread) = FlowsTransmitter::start(self.self_info.clone(), clock_rx, rb_outputs, start_time_rx, current_timestamp.clone(), on_transfer);
    let (shutdown_send, shutdown_recv) = broadcast_queue::channel(16);
    let flows_control_task = tokio::spawn(crate::flows_control_server::run_server(self.self_info.clone(), flows_tx_handle, shutdown_recv));
    self.tx_shutdown_todo = Some(async move {
      shutdown_send.send(()).unwrap();
      flows_control_task.await.unwrap();
      flows_tx_thread.join().unwrap();
    }.boxed());
  }
  pub async fn stop_transmitter(&mut self) {
    self.tx_shutdown_todo.take().unwrap().await;
  }


  pub fn get_realtime_clock_receiver(&self) -> RealTimeClockReceiver {
    async_clock_receiver_to_realtime(self.clock_receiver.subscribe(), self.shared_media_clock.read().unwrap().get_overlay().clone())
  }

  /* pub fn take_tx_inputs(&mut self) -> Vec<RBInput<Sample, P>> {
    unimplemented!()
    //std::mem::take(&mut self.tx_inputs)
  } */

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

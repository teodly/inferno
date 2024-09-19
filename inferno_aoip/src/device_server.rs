use crate::channels_subscriber::{ChannelsBuffering, ChannelsSubscriber, ExternalBuffering, OwnedBuffering};
use crate::flows_tx::FlowsTransmitter;
use crate::info_mcast_server::MulticastMessage;
use crate::mdns_client::MdnsClient;
use crate::media_clock::{async_clock_receiver_to_realtime, make_shared_media_clock, start_clock_receiver, ClockReceiver};
use crate::real_time_box_channel::RealTimeBoxReceiver;
use crate::samples_collector::{RealTimeSamplesReceiver, SamplesCallback, SamplesCollector};
use crate::state_storage::StateStorage;
use crate::ring_buffer::{ExternalBuffer, ExternalBufferParameters, OwnedBuffer, ProxyToBuffer, ProxyToSamplesBuffer, RBInput};
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
use std::env;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};

use std::net::IpAddr;
use std::time::Instant;
use tokio::sync::{broadcast as broadcast_queue, mpsc};

use crate::device_info::{Channel, DeviceInfo};

use crate::{common::*, RealTimeClockReceiver};

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

    DeviceInfo {
      ip_address: my_ipv4,
      board_name: "Inferno-AoIP".to_owned(),
      manufacturer: "Inferno-AoIP".to_owned(),
      model_name: app_name.to_owned(),
      factory_device_id: devid,
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
    }
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
  mdns_client: Arc<MdnsClient>,
  mcast_tx: mpsc::Sender<MulticastMessage>,
  channels_sub_tx: Option<tokio::sync::oneshot::Sender<Arc<ChannelsSubscriber>>>,
  //tx_inputs: Vec<RBInput<Sample, P>>,
  tasks: Vec<JoinHandle<()>>,
  shutdown_todo: VecDeque<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl DeviceServer {
  pub async fn start(self_info: DeviceInfo) -> Self {
    let self_info = Arc::new(self_info);
    let state_storage = Arc::new(StateStorage::new(&self_info));
    let ref_instant = Instant::now();

    let (shutdown_send, shdn_recv1) = broadcast_queue::channel(16);
    let shdn_recv2 = shutdown_send.subscribe();
    let shdn_recv3 = shutdown_send.subscribe();
    let shdn_recv4 = shutdown_send.subscribe();
    let mdns_handle = crate::mdns_server::start_server(self_info.clone());

    let mdns_client = Arc::new(crate::mdns_client::MdnsClient::new(self_info.ip_address));
    let (mcast_tx, mcast_rx) = mpsc::channel(100);

    let clock_receiver = start_clock_receiver();

    info!("waiting for clock");
    clock_receiver.subscribe().recv().await.unwrap();
    info!("clock ready");

    let mut tasks = vec![];
    //let (flows_tx_handle, tx_inputs, flows_tx_thread) = FlowsTransmitter::start(self_info.clone(), clock_rx);

    let (channels_sub_tx, channels_sub_rx) = tokio::sync::oneshot::channel();

    tasks.append(&mut vec![
      tokio::spawn(crate::arc_server::run_server(
        self_info.clone(),
        channels_sub_rx,
        shdn_recv1,
      )),
      tokio::spawn(crate::cmc_server::run_server(self_info.clone(), shdn_recv2)),
      tokio::spawn(crate::info_mcast_server::run_server(self_info.clone(), mcast_rx, shdn_recv3)),
      //tokio::spawn(crate::flows_control_server::run_server(self_info.clone(), flows_tx_handle, shdn_recv4)),
    ]);

    info!("all common tasks spawned");

    let shutdown_todo = async move {
      info!("shutting down");
      shutdown_send.send(()).unwrap();
      mdns_handle.shutdown().unwrap();

      //flows_tx_thread.join().unwrap();
      
    }.boxed();

    Self {
      self_info,
      ref_instant,
      state_storage,
      clock_receiver,
      mdns_client,
      mcast_tx,
      channels_sub_tx: Some(channels_sub_tx),
      tasks,
      //tx_inputs,
      shutdown_todo: VecDeque::from([shutdown_todo])
    }
  }

  pub async fn receive_with_callback(&mut self, samples_callback: SamplesCallback) {
    self.receive(None, |si: &Arc<DeviceInfo>, workers, _| {
      let (sc, future) = SamplesCollector::<OwnedBuffer<Atomic<Sample>>>::new_with_callback(si.clone(), Box::new(samples_callback));
      workers.push(tokio::spawn(future));
      OwnedBuffering::new(524288 /*TODO*/, 4800 /*TODO*/, Arc::new(sc))
    }).await;
  }
  pub async fn receive_realtime(&mut self) -> (RealTimeSamplesReceiver<OwnedBuffer<Atomic<Sample>>>, RealTimeClockReceiver) {
    let mut rt_recv = None;
    let mut clk = None;
    self.receive(None, |si: &Arc<DeviceInfo>, workers, clkrcv: &ClockReceiver| {
      let (col, col_fut, rtr) = SamplesCollector::new_realtime(si.clone(), clkrcv.subscribe());
      rt_recv = Some(rtr);
      clk = Some(clkrcv.subscribe());
      workers.push(tokio::spawn(col_fut));
      OwnedBuffering::new(524288 /*TODO*/, 4800 /*TODO*/, Arc::new(col))
    }).await;

    (rt_recv.unwrap(), async_clock_receiver_to_realtime(clk.unwrap()))
  }
  pub async fn receive_to_external_buffer(&mut self, rx_channels_buffers: Vec<ExternalBufferParameters<Sample>>, start_time_rx: tokio::sync::oneshot::Receiver<Clock>) -> RealTimeClockReceiver {
    let mut clk = None;
    self.receive(Some(start_time_rx), |si: &Arc<DeviceInfo>, workers, clkrcv| {
      clk = Some(clkrcv.subscribe());
      ExternalBuffering::new(rx_channels_buffers, 4800 /*TODO*/)
    }).await;
    
    async_clock_receiver_to_realtime(clk.unwrap())
  }
  async fn receive<P: ProxyToSamplesBuffer + Send + Sync + 'static, B: ChannelsBuffering<P> + Send + Sync + 'static>(&mut self, start_time_rx: Option<tokio::sync::oneshot::Receiver<Clock>>, create_rx_buffering: impl FnOnce(&Arc<DeviceInfo>, &mut Vec<JoinHandle<()>>, &ClockReceiver) -> B) {
    let (flows_rx_handle, flows_rx_thread) = crate::flows_rx::FlowsReceiver::start(self.self_info.clone(), self.ref_instant, start_time_rx);
    let flows_rx_handle = Arc::new(flows_rx_handle);
    let channels_buffering = create_rx_buffering(&self.self_info, &mut self.tasks, &self.clock_receiver);
    let (channels_sub_handle, channels_sub_worker) = ChannelsSubscriber::new(
      self.self_info.clone(),
      make_shared_media_clock(&self.clock_receiver),
      flows_rx_handle.clone(),
      self.mdns_client.clone(),
      self.mcast_tx.clone(),
      channels_buffering,
      self.state_storage.clone(),
      self.ref_instant,
    );
    let channels_sub_handle = Arc::new(channels_sub_handle);
    self.channels_sub_tx.take().expect("DeviceServer::receive called more than once").send(channels_sub_handle.clone()).expect("failed to send ChannelsSubscriber handle");

    self.tasks.push(tokio::spawn(channels_sub_worker));

    let shutdown_todo = async move {
      flows_rx_handle.shutdown().await;
      channels_sub_handle.shutdown().await;
      flows_rx_thread.join().unwrap();
    }.boxed();
    self.shutdown_todo.push_front(shutdown_todo);
  }

  /* pub fn take_tx_inputs(&mut self) -> Vec<RBInput<Sample, P>> {
    unimplemented!()
    //std::mem::take(&mut self.tx_inputs)
  } */

  pub async fn shutdown(self) {
    for todo in self.shutdown_todo {
      todo.await;
    }
    for task in self.tasks {
      task.await.unwrap();
    }
    self.clock_receiver.stop().await.unwrap();
    info!("shutdown ok");
  }
}

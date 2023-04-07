use crate::channels_subscriber::ChannelsSubscriber;
use crate::samples_collector::SamplesCallback;
use crate::state_storage::StateStorage;
use futures::{Future, FutureExt};
use itertools::Itertools;
use tokio::task::JoinHandle;

use std::fs::File;
use std::io::Write;
use std::mem::size_of;
use std::env;
use std::net::Ipv4Addr;
use std::pin::Pin;
use std::sync::Arc;

use std::net::IpAddr;
use std::time::Instant;
use tokio::sync::{broadcast as broadcast_queue, mpsc};

use crate::device_info::{Channel, DeviceInfo};

use crate::common::*;

pub trait SelfInfoBuilder {
  fn new_self(app_name: &str, short_app_name: &str, my_ip: Option<Ipv4Addr>) -> DeviceInfo;
  fn make_rx_channels(self, count: usize) -> DeviceInfo;
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
      pcm_type: 0xe,
      sample_rate,
    }
  }
  fn make_rx_channels(mut self, count: usize) -> DeviceInfo {
    self.rx_channels = (1..=count)
      .map(|id| Channel { factory_name: format!("{id:02}"), friendly_name: format!("RX {id}") })
      .collect_vec();
    self
  }
}

pub struct DeviceServer {
  self_info: Arc<DeviceInfo>,
  shutdown_todo: Pin<Box<dyn Future<Output = ()> + Send>>
}

impl DeviceServer {
  pub async fn start(self_info: DeviceInfo, samples_callback: SamplesCallback) -> Self {
    let state_storage = Arc::new(StateStorage::new(&self_info));
    let self_info = Arc::new(self_info);
    let ref_instant = Instant::now();

    let (shutdown_send, shdn_recv1) = broadcast_queue::channel(16);
    let shdn_recv2 = shutdown_send.subscribe();
    let shdn_recv3 = shutdown_send.subscribe();
    let mdns_handle = crate::mdns_server::start_server(self_info.clone());
    let (flows_handle, flows_thread) = crate::flows_rx::FlowsReceiver::start(self_info.clone(), ref_instant);
    let flows_handle = Arc::new(flows_handle);
    let mdns_client = Arc::new(crate::mdns_client::MdnsClient::new(self_info.ip_address));
    let (mcast_tx, mcast_rx) = mpsc::channel(100);

    let (samples_collector, samples_collector_worker) =
      crate::samples_collector::SamplesCollector::new(self_info.clone(), Box::new(samples_callback));
    let samples_collector = Arc::new(samples_collector);
    let (channels_sub_handle, channels_sub_worker) = ChannelsSubscriber::new(
      self_info.clone(),
      flows_handle.clone(),
      mdns_client,
      mcast_tx,
      samples_collector.clone(),
      state_storage,
      ref_instant,
    );
    let channels_sub_handle = Arc::new(channels_sub_handle);
    let tasks = [
      tokio::spawn(crate::arc_server::run_server(
        self_info.clone(),
        channels_sub_handle.clone(),
        shdn_recv1,
      )),
      tokio::spawn(crate::cmc_server::run_server(self_info.clone(), shdn_recv2)),
      tokio::spawn(crate::info_mcast_server::run_server(self_info.clone(), mcast_rx, shdn_recv3)),
      tokio::spawn(channels_sub_worker),
      tokio::spawn(samples_collector_worker),
    ];

    info!("all tasks spawned");

    let channels_sub_handle1 = channels_sub_handle.clone();
    let shutdown_todo = async move {
      info!("shutting down");
      shutdown_send.send(()).unwrap();
      mdns_handle.shutdown().unwrap();
      flows_handle.shutdown().await;
      samples_collector.shutdown().await;
      channels_sub_handle1.shutdown().await;
      for task in tasks {
        task.await.unwrap();
      }
      flows_thread.join().unwrap();
      info!("shutdown ok");
    }.boxed();

    Self {
      self_info,
      shutdown_todo
    }
  }

  pub async fn shutdown(self) {
    self.shutdown_todo.await;
  }
}

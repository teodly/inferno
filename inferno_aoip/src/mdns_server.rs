use searchfire::{
  broadcast::{BroadcasterBuilder, BroadcasterHandle, ServiceBuilder},
  dns::rr::Name,
  net::{IpVersion, TargetInterface},
};
use std::{net::IpAddr, sync::Arc};

use crate::device_info::DeviceInfo;
use crate::protocol::{proto_arc::PORT as ARC_PORT, proto_cmc::PORT as CMC_PORT, flows_control::PORT as FLOWS_CONTROL_PORT};
use crate::flows_tx::{FPP_MIN, FPP_MAX, MAX_CHANNELS_IN_FLOW};

fn kv<T: std::fmt::Display>(key: &str, value: T) -> String {
  format!("{key}={value}")
}

pub fn start_server(self_info: Arc<DeviceInfo>) -> BroadcasterHandle {
  let service_type = |st| -> Name {
    Name::from_labels([st, "_udp", "local"].iter().map(|&s| s.as_bytes())).unwrap()
  };
  let hostname = Name::from_labels([self_info.friendly_hostname.as_bytes()]).unwrap();
  let mut bb = BroadcasterBuilder::new()
    //.loopback()
    .interface_v4(TargetInterface::Specific(self_info.ip_address))
    .add_service(
      ServiceBuilder::new(service_type("_netaudio-arc"), hostname.clone(), ARC_PORT)
        .unwrap()
        .add_ip_address(IpAddr::V4(self_info.ip_address))
        .add_txt_truncated("arcp_vers=2.7.41")
        .add_txt_truncated("arcp_min=0.2.4")
        .add_txt_truncated("router_vers=4.0.2")
        .add_txt_truncated(kv("router_info", &self_info.board_name))
        .add_txt_truncated(kv("mf", &self_info.manufacturer))
        .add_txt_truncated(kv("model", &self_info.model_number))
        .ttl(4500)
        .build()
        .unwrap(),
    )
    .add_service(
      ServiceBuilder::new(service_type("_netaudio-cmc"), hostname, CMC_PORT)
        .unwrap()
        .add_ip_address(IpAddr::V4(self_info.ip_address))
        .add_txt_truncated(kv("id", &hex::encode(self_info.factory_device_id)))
        .add_txt_truncated("process=0") // ??? maybe for Dante Via ???
        .add_txt_truncated("cmcp_vers=1.2.0")
        .add_txt_truncated("cmcp_min=1.0.0")
        .add_txt_truncated("server_vers=4.0.2")
        .add_txt_truncated("channels=0x6000004d") // ???
        .add_txt_truncated(kv("mf", &self_info.manufacturer))
        .add_txt_truncated(kv("model", &self_info.model_number))
        .add_txt_truncated("") // really needed?
        .add_txt_truncated("") // really needed?
        .build()
        .unwrap(),
    );
  
  for (index, txch) in self_info.tx_channels.iter().enumerate() {
    let service = |ch_name: &str, default: bool| {
      let name = Name::from_labels([format!("{}@{}", ch_name, self_info.friendly_hostname).as_bytes()]).unwrap();
      let mut b = ServiceBuilder::new(service_type("_netaudio-chan"), name, FLOWS_CONTROL_PORT)
        .unwrap()
        .add_ip_address(IpAddr::V4(self_info.ip_address))
        .add_txt_truncated("txtvers=2")
        .add_txt_truncated("dbcp1=0x1102")
        .add_txt_truncated("dbcp=0x1004")
        .add_txt_truncated(kv("id", index+1))
        .add_txt_truncated(kv("rate", self_info.sample_rate))
        .add_txt_truncated(format!("pcm={} {:x}", self_info.bits_per_sample/8, self_info.pcm_type))
        .add_txt_truncated(kv("enc", self_info.bits_per_sample))
        .add_txt_truncated(kv("en", self_info.bits_per_sample))
        .add_txt_truncated(kv("latency_ns", self_info.latency_ns))
        .add_txt_truncated(format!("fpp={},{}", FPP_MAX, FPP_MIN))
        .add_txt_truncated(kv("nchan", MAX_CHANNELS_IN_FLOW.min(self_info.tx_channels.len() as u16)));
      if default {
        b = b.add_txt_truncated("default");
      }
      b.build().unwrap()
    };
    bb = bb.add_service(service(&txch.factory_name, true));
    if txch.factory_name != txch.friendly_name {
      bb = bb.add_service(service(&txch.friendly_name, false));
    }
  }
  
  bb.build(IpVersion::V4)
    .unwrap()
    .run_in_background()
    // TODO it doesn't work when there is no default gateway in routing table
    // thread 'main' panicked at 'called `Result::unwrap()` on an `Err` value: MultiIpIoError(V4(Os { code: 101, kind: NetworkUnreachable, message: "Network is unreachable" }))', inferno_aoip/src/mdns_server.rs:55:6
}

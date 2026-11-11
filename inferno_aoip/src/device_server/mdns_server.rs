use searchfire::{
  broadcast::{BroadcasterBuilder, BroadcasterHandle, ServiceBuilder},
  dns::rr::Name,
  net::{IpVersion, TargetInterface},
};
use std::{
  collections::BTreeMap,
  net::{IpAddr, Ipv4Addr},
  sync::{Arc, RwLock},
};

use super::flows_tx::{FPP_MAX_ADVERTISED, FPP_MIN, MAX_CHANNELS_IN_FLOW};
use crate::{
  device_info::DeviceInfo,
  mdns_client::{self_origin_from_self_info, MdnsClient, PointerToMulticast},
  utils::LogAndForget,
};

pub struct DeviceMDNSResponder {
  handle: RwLock<Option<BroadcasterHandle>>,
  self_info: Arc<DeviceInfo>,
  tx_latency_ns: u32,
  self_origin: Vec<u8>,
  multicasts_by_channel: Arc<RwLock<BTreeMap<usize, PointerToMulticast>>>,
}

pub fn kv<T: std::fmt::Display>(key: &str, value: T) -> String {
  format!("{key}={value}")
}

pub fn service_type(st: &str) -> Name {
  Name::from_labels([st, "_udp", "local"].iter().map(|&s| s.as_bytes())).unwrap()
}
pub fn in_addr_type() -> Name {
  Name::from_labels(["in-addr", "local"].iter().map(|&s| s.as_bytes())).unwrap()
}
fn multicast_ip_to_name(addr: Ipv4Addr) -> Name {
  let octets = addr.octets();
  Name::from_labels(
    [&octets[3].to_string(), &octets[2].to_string(), &octets[1].to_string(), &octets[0].to_string()]
      .iter()
      .map(|&s| s.as_bytes()),
  )
  .unwrap()
}

impl DeviceMDNSResponder {
  pub fn start(
    self_info: Arc<DeviceInfo>,
    tx_latency_ns: u32,
    multicasts_by_channel: Arc<RwLock<BTreeMap<usize, PointerToMulticast>>>,
  ) -> Self {
    let hostname = Name::from_labels([self_info.friendly_hostname.as_bytes()]).unwrap();
    let bb = BroadcasterBuilder::new()
      .loopback()
      .interface_v4(TargetInterface::Specific(self_info.ip_address))
      .add_service(
        ServiceBuilder::new(service_type("_netaudio-arc"), hostname.clone(), self_info.arc_port)
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
        ServiceBuilder::new(service_type("_netaudio-cmc"), hostname, self_info.cmc_port)
          .unwrap()
          .add_ip_address(IpAddr::V4(self_info.ip_address))
          .add_txt_truncated(kv("id", &hex::encode(self_info.factory_device_id)))
          .add_txt_truncated(kv("process", self_info.process_id))
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

    let handle = bb.build(IpVersion::V4).unwrap().run_in_background();

    Self {
      handle: RwLock::new(Some(handle)),
      self_origin: self_origin_from_self_info(&self_info),
      self_info,
      tx_latency_ns,
      multicasts_by_channel,
    }
  }

  /// `<label>@<hostname>` as a single DNS label. Channel labels come from rename
  /// requests and saved state; a label over 63 bytes is refused (logged), not unwrapped.
  fn instance_name(&self, label: &str) -> Option<Name> {
    let instance = format!("{}@{}", label, self.self_info.friendly_hostname);
    match Name::from_labels([instance.as_bytes()]) {
      Ok(name) => Some(name),
      Err(e) => {
        log::error!("cannot advertise {instance:?}: {e}");
        None
      }
    }
  }

  pub fn add_tx_channel(&self, index: usize) {
    let self_info = &*self.self_info;
    let txch = match self_info.tx_channels.get(index) {
      Some(ch) => ch,
      None => {
        log::error!("trying to advertise nonexisting TX channel index {index}");
        return;
      }
    };
    let bundle = self
      .multicasts_by_channel
      .read()
      .unwrap()
      .get(&index)
      .map(|p| format!("b.{}={}", p.bundle_id, p.channel_in_bundle + 1));
    let service = |ch_name: &str, default: bool| {
      let name = self.instance_name(ch_name)?;
      let builder =
        ServiceBuilder::new(service_type("_netaudio-chan"), name, self_info.flows_control_port)
          .map_err(|e| log::error!("cannot advertise TX channel {ch_name:?}: {e:?}"))
          .ok()?;
      let mut b = builder
        .add_ip_address(IpAddr::V4(self_info.ip_address))
        .add_txt_truncated("txtvers=2")
        .add_txt_truncated("dbcp1=0x1102")
        .add_txt_truncated("dbcp=0x1004")
        .add_txt_truncated(kv("id", index + 1))
        .add_txt_truncated(kv("rate", self_info.sample_rate))
        .add_txt_truncated(format!("pcm={} {:x}", self_info.bits_per_sample / 8, self_info.pcm_type))
        .add_txt_truncated(kv("enc", self_info.bits_per_sample))
        .add_txt_truncated(kv("en", self_info.bits_per_sample))
        .add_txt_truncated(kv("latency_ns", self.tx_latency_ns))
        .add_txt_truncated(format!("fpp={},{}", FPP_MAX_ADVERTISED, FPP_MIN))
        .add_txt_truncated(kv("nchan", MAX_CHANNELS_IN_FLOW.min(self_info.tx_channels.len() as u16)));
      if default {
        b = b.add_txt_truncated("default");
      }
      if let Some(s) = &bundle {
        b = b.add_txt_truncated(s.clone());
      }
      b.build().map_err(|e| log::error!("cannot advertise TX channel {ch_name:?}: {e:?}")).ok()
    };
    let handle = self.handle.read().unwrap();
    match handle.as_ref() {
      Some(handle) => {
        if let Some(svc) = service(&txch.factory_name, true) {
          handle.add_service(svc).log_and_forget();
        }
        let friendly_name_locked = txch.friendly_name.read();
        let friendly_name = friendly_name_locked.unwrap();
        if txch.factory_name != *friendly_name {
          if let Some(svc) = service(&friendly_name, false) {
            handle.add_service(svc).log_and_forget();
          }
        }
      }
      None => {
        log::error!("BUG: trying to add channel using BroadcasterHandle after it was shut down");
      }
    };
  }

  pub fn remove_tx_channel(&self, index: usize) {
    let self_info = &*self.self_info;
    let remove = |ch_name: &str| {
      let name = match self.instance_name(ch_name) {
        Some(name) => name,
        None => return,
      };
      match self.handle.read().unwrap().as_ref() {
        Some(handle) => {
          handle.remove_named_service(service_type("_netaudio-chan"), name).log_and_forget();
        }
        None => {
          log::error!("BUG: trying to remove channel using BroadcasterHandle after it was shut down");
        }
      }
    };
    let txch = match self_info.tx_channels.get(index) {
      Some(ch) => ch,
      None => {
        log::error!("trying to withdraw nonexisting TX channel index {index}");
        return;
      }
    };
    remove(&txch.factory_name);
    let friendly_name_locked = txch.friendly_name.read();
    let friendly_name = friendly_name_locked.unwrap();
    if txch.factory_name != *friendly_name {
      remove(&friendly_name);
    }
  }

  pub fn add_multicast_bundle(
    &self,
    bundle_id: usize,
    channels_per_flow: usize,
    fpp: usize,
    dst_addr: Ipv4Addr,
    dst_port: u16,
  ) {
    let self_info = &*self.self_info;
    let name = match self.instance_name(&bundle_id.to_string()) {
      Some(name) => name,
      None => return,
    };
    let handle = self.handle.read().unwrap();
    let service =
      match ServiceBuilder::new(service_type("_netaudio-bund"), name, self_info.flows_control_port) {
        Ok(b) => b,
        Err(e) => {
          log::error!("cannot advertise multicast bundle {bundle_id}: {e:?}");
          return;
        }
      }
      .add_ip_address(IpAddr::V4(self_info.ip_address))
      .add_txt_truncated("txtvers=1")
      .add_txt_truncated(kv("id", bundle_id))
      .add_txt_truncated(kv("nchan", channels_per_flow))
      .add_txt_truncated(kv("latency_ns", self.tx_latency_ns))
      .add_txt_truncated(kv("fpp", fpp))
      .add_txt_truncated(kv("rate", self_info.sample_rate))
      .add_txt_truncated(kv("enc", self_info.bits_per_sample))
      .add_txt_truncated(kv("a.0", dst_addr))
      .add_txt_truncated(kv("p.0", dst_port))
      .build();
    let service = match service {
      Ok(s) => s,
      Err(e) => {
        log::error!("cannot advertise multicast bundle {bundle_id}: {e:?}");
        return;
      }
    };

    match handle.as_ref() {
      Some(handle) => {
        handle.add_service(service).log_and_forget();
      }
      None => {
        log::error!("BUG: trying to add multicast bundle using BroadcasterHandle after it was shut down");
      }
    }
  }

  pub fn remove_multicast_bundle(&self, bundle_id: usize) {
    let name = match self.instance_name(&bundle_id.to_string()) {
      Some(name) => name,
      None => return,
    };
    let handle = self.handle.read().unwrap();
    match handle.as_ref() {
      Some(handle) => {
        handle.remove_named_service(service_type("_netaudio-bund"), name).log_and_forget();
      }
      None => {
        log::error!(
          "BUG: trying to remove multicast bundle using BroadcasterHandle after it was shut down"
        );
      }
    }
  }

  pub fn reserve_multicast_ip(&self, addr: Ipv4Addr) {
    let self_info = &*self.self_info;
    let b = ServiceBuilder::new(in_addr_type(), multicast_ip_to_name(addr), 0)
      .unwrap()
      .add_ip_address(IpAddr::V4(self_info.ip_address))
      .add_additional_txt(
        Name::from_labels(["_inferno-response-origin", "local"]).unwrap(),
        self.self_origin.clone(),
      );
    let handle = self.handle.read().unwrap();
    match handle.as_ref() {
      Some(handle) => {
        handle.add_service(b.build().unwrap()).log_and_forget();
      }
      None => {
        log::error!("BUG: trying to reserve multicast IP using BroadcasterHandle after it was shut down");
      }
    }
  }
  pub fn remove_multicast_ip(&self, addr: Ipv4Addr) {
    let handle = self.handle.read().unwrap();
    match handle.as_ref() {
      Some(handle) => {
        handle.remove_named_service(in_addr_type(), multicast_ip_to_name(addr)).log_and_forget();
      }
      None => {
        log::error!(
          "BUG: trying to remove multicast IP reservation using BroadcasterHandle after it was shut down"
        );
      }
    }
  }

  pub fn shutdown_and_join(&self) {
    self
      .handle
      .write()
      .unwrap()
      .take()
      .expect("shutting down more than once")
      .shutdown()
      .log_and_forget();
  }
}

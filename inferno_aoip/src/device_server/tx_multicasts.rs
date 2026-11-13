use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize};
use std::sync::RwLock;
use std::{net::Ipv4Addr, sync::Arc};

use itertools::Itertools;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::device_info::DeviceInfo;
use crate::device_server::flows_tx::FPP_MAX_ADVERTISED;
use crate::mdns_client::{MdnsClient, PointerToMulticast};
use crate::state_storage::StateStorage;
use crate::utils::LogAndForget;

use super::flows_tx::FlowInfo as TXFlowInfo;
use super::flows_tx::{FlowsTransmitter, MAX_FLOWS};
use super::mdns_server::DeviceMDNSResponder;

pub const MEDIA_PORT: u16 = 4321;

#[derive(Deserialize, Serialize)]
pub struct SavedMulticastTXFlow {
  flow_id: usize,
  dst_addr: Ipv4Addr,
  dst_port: u16,
  local_channel_ids: Vec<usize>,
}

#[derive(Deserialize, Serialize)]
pub struct SavedMulticastTXFlows {
  flows: Vec<SavedMulticastTXFlow>,
}

struct Bundle {
  local_channel_indices: Vec<Option<usize>>,
  dst_addr: Arc<AtomicU32>,
}

pub struct TransmitMulticasts {
  bundles: Arc<Mutex<Vec<Option<Bundle>>>>,
  multicasts_by_channel: Arc<RwLock<BTreeMap<usize, PointerToMulticast>>>,
  should_work: Arc<AtomicBool>,
  state_storage: Arc<StateStorage>,
  self_info: Arc<DeviceInfo>,
  flows_tx: Arc<Mutex<Option<FlowsTransmitter>>>,
  mdns_server: Arc<DeviceMDNSResponder>,
  mdns_client: Arc<MdnsClient>,
}

impl TransmitMulticasts {
  pub fn new(
    multicasts_by_channel: Arc<RwLock<BTreeMap<usize, PointerToMulticast>>>,
    state_storage: Arc<StateStorage>,
    self_info: Arc<DeviceInfo>,
    flows_tx: Arc<Mutex<Option<FlowsTransmitter>>>,
    mdns_server: Arc<DeviceMDNSResponder>,
    mdns_client: Arc<MdnsClient>,
  ) -> Self {
    Self {
      bundles: Arc::new(Mutex::new((0..MAX_FLOWS).map(|_| None).collect_vec())),
      multicasts_by_channel,
      should_work: Arc::new(true.into()),
      state_storage,
      self_info,
      flows_tx,
      mdns_server,
      mdns_client,
    }
  }
  pub async fn load_state(&self) {
    let loaded = self
      .state_storage
      .load::<SavedMulticastTXFlows>("tx_multicasts")
      .map(|s| s.flows)
      .unwrap_or(vec![]);
    for saved in loaded {
      if saved.flow_id == 0 {
        continue;
      }
      if saved.dst_port != MEDIA_PORT {
        error!(
          "non-default port specified in saved state: {}, changing to {}",
          saved.dst_port, MEDIA_PORT
        );
      }
      let mut flow_index = saved.flow_id - 1;
      {
        let bundles = self.bundles.lock().await;
        if flow_index >= bundles.len() || bundles[flow_index].is_some() {
          if let Some(new_index) = bundles.iter().position(|b| b.is_none()) {
            warn!("loading multicast flows: changing id {} to {}", flow_index + 1, new_index + 1);
            flow_index = new_index;
          } else {
            error!(
              "unable to read multicast flow from config: too many flows defined, {MAX_FLOWS} allowed"
            );
            continue;
          }
        }
      }
      self
        .add_flow_internal(
          flow_index,
          saved
            .local_channel_ids
            .iter()
            .map(|&id| if id == 0 { None } else { Some(id - 1) })
            .collect_vec(),
          saved.dst_addr,
        )
        .await
        .log_and_forget();
    }
  }
  pub async fn add_flow(
    &self,
    flow_index: usize,
    channel_indices: Vec<Option<usize>>,
  ) -> Result<(), std::io::Error> {
    self.add_flow_internal(flow_index, channel_indices, Ipv4Addr::UNSPECIFIED).await
  }
  async fn add_flow_internal(
    &self,
    flow_index: usize,
    channel_indices: Vec<Option<usize>>,
    preferred_address: Ipv4Addr,
  ) -> Result<(), std::io::Error> {
    info!("adding flow index {flow_index} with local channel indices {channel_indices:?}");
    let bytes_per_sample = (self.self_info.bits_per_sample / 8).try_into().unwrap();
    let dst_addr_arc: Arc<AtomicU32> = Arc::new(0.into());
    let fpp = FPP_MAX_ADVERTISED.try_into().unwrap() /* TODO */;
    let dst = {
      let mut bundles = self.bundles.lock().await;
      match bundles.get(flow_index) {
        None => {
          error!("multicast flow index {flow_index} out of range");
          return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
        Some(Some(_)) => {
          error!("multicast flow index {flow_index} already in use");
          return Err(std::io::Error::from(std::io::ErrorKind::AddrInUse));
        }
        Some(None) => {}
      }
      let mut flows_tx = self.flows_tx.lock().await;
      let (dst_addr, dst_port) = if let Some(tx) = flows_tx.as_mut() {
        let (dst_addr, dst_port) = if preferred_address.is_unspecified() {
          tx.random_multicast_destination()
        } else {
          (preferred_address, MEDIA_PORT)
        };
        let flow_info = TXFlowInfo {
          rx_hostname: None,
          rx_flow_name: None,
          dst_addr,
          dst_port,
          local_channel_indices: channel_indices.clone(),
        };
        tx.add_flow(
          flow_info,
          fpp,
          (self.self_info.bits_per_sample / 8).try_into().unwrap(),
          Some(flow_index.try_into().unwrap()),
          true,
        )
        .await?;
        info!("added multicast flow, waiting grace period...");
        (dst_addr, dst_port)
      } else {
        error!("BUG: TransmitMulticasts::add_flow called but flows_tx is None. this flow will start working only after you restart Inferno");
        (Ipv4Addr::UNSPECIFIED, 0)
      };
      bundles[flow_index] =
        Some(Bundle { local_channel_indices: channel_indices.clone(), dst_addr: dst_addr_arc.clone() });
      (dst_addr, dst_port)
    };
    self.save_state().await;
    if !dst.0.is_unspecified() {
      let mdns_client = self.mdns_client.clone();
      let mdns_server = self.mdns_server.clone();
      let flows_tx = self.flows_tx.clone();
      let should_work = self.should_work.clone();
      let multicasts_by_channel = self.multicasts_by_channel.clone();
      tokio::spawn(async move {
        let mut dst_addr = dst.0;
        let mut dst_port = dst.1;
        loop {
          let mut ok = true;
          ok &= !mdns_client.is_multicast_ip_already_used(dst_addr).await.unwrap_or(true);
          if ok {
            mdns_server.reserve_multicast_ip(dst_addr);
            ok &= !mdns_client.is_multicast_ip_already_used(dst_addr).await.unwrap_or(true);
          }
          {
            let mut flows_tx_opt = flows_tx.lock().await;
            if let Some(flows_tx) = flows_tx_opt.as_mut() {
              if !should_work.load(std::sync::atomic::Ordering::SeqCst) {
                // abort whatever is being done, the flows_tx will be destroyed soon...
                warn!("flows_tx will be destroyed soon, not activating transmitter");
                break;
              }
              if ok {
                if !flows_tx.activate_multicast_flow(flow_index.try_into().unwrap()) {
                  warn!("multicast flow id {} was deleted during its grace period", flow_index + 1);
                  break;
                }
                info!("no multicast conflict detected, activated transmitter");
                dst_addr_arc.store(dst_addr.to_bits(), std::sync::atomic::Ordering::SeqCst);
                {
                  let mut mcasts = multicasts_by_channel.write().unwrap();
                  for (channel_in_bundle, chi_opt) in channel_indices.iter().enumerate() {
                    if let Some(chi) = chi_opt {
                      mcasts.insert(
                        *chi,
                        PointerToMulticast {
                          tx_hostname: "".to_owned(),
                          bundle_id: flow_index + 1,
                          channel_in_bundle,
                        },
                      );
                    }
                  }
                }
                mdns_server.add_multicast_bundle(
                  flow_index + 1,
                  channel_indices.len(),
                  fpp,
                  dst_addr,
                  dst_port,
                );

                // refresh channels so that multicast will be advertised
                for index_opt in &channel_indices {
                  if let Some(index) = index_opt {
                    mdns_server.remove_tx_channel(*index);
                    mdns_server.add_tx_channel(*index);
                  }
                }

                // FIXME: we can't do self.save_state().await because we have no self here!
                // so multicast address won't be saved, sorry
                // TODO: refactor to make it possible

                break;
              } else {
                warn!("multicast address conflict detected: {dst_addr:?}, retrying");
                flows_tx.remove_multicast_flow(flow_index.try_into().unwrap()).await.log_and_forget();
                (dst_addr, dst_port) = flows_tx.random_multicast_destination();

                // note: the following add_flow must happen after remove_multicast_flow, WITHOUT flows_tx being unlocked in the meantime!
                // otherwise race condition may happen - our flow_index may get occupied by other, unrelated flow

                // TODO: DRY
                let flow_info = TXFlowInfo {
                  rx_hostname: None,
                  rx_flow_name: None,
                  dst_addr,
                  dst_port,
                  local_channel_indices: channel_indices.clone(),
                };
                flows_tx
                  .add_flow(flow_info, fpp, bytes_per_sample, Some(flow_index.try_into().unwrap()), true)
                  .await
                  .log_and_forget();
              }
            } else {
              error!("trying to activate multicast flow but we have no flows transmitter active");
              break;
            }
          }
        }
      });
    }
    Ok(())
  }
  pub async fn remove_flow(&self, flow_index: usize) -> Result<(), std::io::Error> {
    self.remove_flow_internal(flow_index, false).await?;
    self.save_state().await;
    Ok(())
  }
  async fn remove_flow_internal(
    &self,
    flow_index: usize,
    ignore_nonexisting: bool,
  ) -> Result<(), std::io::Error> {
    let mut bundles = self.bundles.lock().await;
    if let Some(bundle) = bundles.get_mut(flow_index).and_then(Option::take) {
      let mut flows_tx_opt = self.flows_tx.lock().await;
      if let Some(flows_tx) = flows_tx_opt.as_mut() {
        if !self.should_work.load(std::sync::atomic::Ordering::SeqCst) {
          // flows_tx will be destroyed soon or was already changed
          return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
        }
        let addr_int = bundle.dst_addr.load(std::sync::atomic::Ordering::SeqCst);
        if addr_int != 0 {
          self.mdns_server.remove_multicast_bundle(flow_index + 1);
          self.mdns_server.remove_multicast_ip(Ipv4Addr::from_bits(addr_int));
        }
        let result = flows_tx.remove_multicast_flow(flow_index.try_into().unwrap()).await;
        {
          let mut mcasts = self.multicasts_by_channel.write().unwrap();
          for (_channel_in_bundle, chi_opt) in bundle.local_channel_indices.iter().enumerate() {
            if let Some(chi) = chi_opt {
              mcasts.remove(chi);
            }
          }
        }
        // refresh channels so that multicast will no longer be advertised
        for index_opt in bundle.local_channel_indices {
          if let Some(index) = index_opt {
            self.mdns_server.remove_tx_channel(index);
            self.mdns_server.add_tx_channel(index);
          }
        }
        result
      } else {
        // flows_tx has been destroyed
        Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
      }
    } else if !ignore_nonexisting {
      error!("trying to remove nonexisting multicast TX flow index {flow_index}");
      Err(std::io::Error::from(std::io::ErrorKind::NotFound))
    } else {
      Ok(())
    }
  }
  pub async fn shutdown(&self) {
    {
      let _flows_tx_opt = self.flows_tx.lock().await;
      self.should_work.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    for i in 0..MAX_FLOWS {
      self.remove_flow_internal(i.try_into().unwrap(), true).await.log_and_forget();
    }
  }
  pub async fn save_state(&self) {
    if !self.should_work.load(std::sync::atomic::Ordering::SeqCst) {
      return;
    }
    let to_save = SavedMulticastTXFlows {
      flows: self
        .bundles
        .lock()
        .await
        .iter()
        .enumerate()
        .filter_map(|(flow_index, bundle_opt)| {
          bundle_opt.as_ref().map(|bundle| SavedMulticastTXFlow {
            flow_id: flow_index + 1,
            dst_addr: Ipv4Addr::from_bits(bundle.dst_addr.load(std::sync::atomic::Ordering::SeqCst)),
            dst_port: MEDIA_PORT,
            local_channel_ids: bundle
              .local_channel_indices
              .iter()
              .map(|index_opt| index_opt.map(|i| i + 1).unwrap_or(0))
              .collect_vec(),
          })
        })
        .collect_vec(),
    };
    self.state_storage.save("tx_multicasts", &to_save).log_and_forget();
  }
  pub fn get_multicast_by_channel(&self, channel_index: usize) -> Option<PointerToMulticast> {
    self.multicasts_by_channel.read().unwrap().get(&channel_index).map(|p| p.clone())
  }
}

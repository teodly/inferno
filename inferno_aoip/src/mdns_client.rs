use crate::common::*;
use std::{
  collections::BTreeMap,
  error::Error,
  io,
  net::{Ipv4Addr, SocketAddr},
  str::{self},
};

use searchfire::{
  discovery::DiscoveryBuilder,
  dns::{
    op::DnsResponse,
    rr::{Name, RecordType},
  },
  net::{IpVersion, TargetInterface},
};

pub type TxtEntries = BTreeMap<String, String>;

#[derive(Debug)]
pub struct AdvertisedService {
  pub addr: SocketAddr,
  pub properties: TxtEntries,
}

#[derive(Debug, Clone)]
pub struct PointerToMulticast {
  pub bundle_id: usize,
  pub channel_in_bundle: usize,
}

#[derive(Debug, Clone)]
pub struct AdvertisedChannel {
  pub addr: SocketAddr,
  pub tx_channels_per_flow: usize,
  pub tx_channel_id: u16,
  pub bits_per_sample: u32,
  pub dbcp1: u16,
  /// minimum Frames Per Packet supported by transmitter (Frame = single samples of all channels)
  pub fpp_min: u16,
  /// maximum Frames Per Packet supported by transmitter (Frame = single samples of all channels)
  pub fpp_max: u16,
  pub min_rx_latency_ns: usize,
  pub multicast: Option<PointerToMulticast>,
}

pub struct MdnsClient {
  listen_ip: Ipv4Addr,
}

impl MdnsClient {
  pub fn new(listen_ip: Ipv4Addr) -> Self {
    Self { listen_ip }
  }
  async fn do_single_query(
    &self,
    types: &[RecordType],
    fqdn: &Name,
  ) -> Result<DnsResponse, Box<dyn Error>> {
    DiscoveryBuilder::new()
      .interface_v4(TargetInterface::Specific(self.listen_ip))
      .build(IpVersion::V4)
      .map_err(|e| Box::new(e))?
      .single_query(types, fqdn)
      .await
      .map_err(|e| Box::new(e).into())
  }

  pub async fn query(&self, fqdn_parts: &[&str]) -> Result<AdvertisedService, Box<dyn Error>> {
    debug!("resolving {fqdn_parts:?}");
    let fqdn =
      Name::from_labels(fqdn_parts.iter().map(|&s| s.as_bytes())).map_err(|e| Box::new(e))?;
    let response = self.do_single_query(&[RecordType::SRV, RecordType::TXT], &fqdn).await?;
    let mut target = None;
    let mut properties = BTreeMap::new();
    for record in response.answers() {
      if record.name().to_lowercase() != fqdn.to_lowercase() {
        continue;
      }
      if let Some(rdata) = record.data() {
        if let Some(srv) = rdata.as_srv() {
          target = Some((srv.target(), srv.port()));
        } else if let Some(txt) = rdata.as_txt() {
          for txtbytes in txt.txt_data().iter() {
            let s = str::from_utf8(txtbytes).map_err(Box::new)?;
            if let Some((key, value)) = s.split_once("=") {
              properties.insert(key.to_owned(), value.to_owned());
            }
          }
        }
      }
    }
    if let Some((name, port)) = target {
      for record in response.additionals() {
        if record.name().to_lowercase() != name.to_lowercase() {
          continue;
        }
        if let Some(rdata) = record.data() {
          if let Some(a) = rdata.as_a() {
            return Ok(AdvertisedService {
              addr: SocketAddr::new(std::net::IpAddr::V4(*a), port),
              properties,
            });
          }
        }
      }
    }
    return Err(Box::new(io::Error::from(io::ErrorKind::NotFound)));
  }

  pub async fn query_chan(&self, full_name: &str) -> Result<AdvertisedChannel, Box<dyn Error>> {
    let fqdn = [full_name, "_netaudio-chan", "_udp", "local"];
    let result = self.query(&fqdn).await?;
    let parse_int = |key| -> Result<usize, Box<dyn Error>> {
      match result.properties.get(key) {
        Some(s) => {
          let result = if s.starts_with("0x") {
            usize::from_str_radix(&s[2..], 16)
          } else {
            s.parse::<usize>()
          };
          match result {
            Ok(v) => Ok(v),
            Err(e) => {
              error!("unable to parse {key}={s}");
              return Err(Box::new(e));
            }
          }
        }
        None => {
          error!("{key} not found in dns response");
          return Err(Box::new(io::Error::from(io::ErrorKind::NotFound)));
        }
      }
    };
    let mut multicast = None;
    for (key, value) in &result.properties {
      if key.starts_with("b.") {
        multicast = Some(PointerToMulticast {
          bundle_id: match key[2..].parse::<usize>() {
            Ok(v) => v,
            Err(_e) => {
              error!("Unable to parse multicast bundle key {key}");
              break;
            }
          },
          channel_in_bundle: match value.parse::<usize>() {
            Ok(v) => v,
            Err(_e) => {
              error!("Unable to parse multicast bundle value {value}");
              break;
            }
          },
        });
        break;
      }
    }
    let (fpp1, fpp2) = result.properties.get("fpp").ok_or(Box::new(io::Error::from(io::ErrorKind::NotFound)))?.split_once(",").ok_or(Box::new(io::Error::from(io::ErrorKind::InvalidData)))?;
    return Ok(AdvertisedChannel {
      addr: result.addr,
      tx_channels_per_flow: parse_int("nchan")?,
      tx_channel_id: parse_int("id")? as u16,
      bits_per_sample: parse_int("enc").or(parse_int("en"))? as u32,
      dbcp1: parse_int("dbcp1")? as u16,
      fpp_min: fpp2.parse()?,
      fpp_max: fpp1.parse()?,
      min_rx_latency_ns: parse_int("latency_ns")?,
      multicast,
    });
  }
}

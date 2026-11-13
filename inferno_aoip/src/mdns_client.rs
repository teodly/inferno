use crate::{common::*, device_info::DeviceInfo};
use std::{
  collections::BTreeMap,
  error::Error,
  io,
  net::{Ipv4Addr, SocketAddr, SocketAddrV4},
  str::{self},
  sync::Arc,
  time::Duration,
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
  pub tx_hostname: String,
  pub bundle_id: usize,
  pub channel_in_bundle: usize,
}

#[derive(Debug, Clone)]
pub struct AdvertisedBundle {
  pub tx_channels_per_flow: usize,
  pub tx_bundle_id: u16,
  pub bits_per_sample: u32,
  pub fpp: u16,
  pub min_rx_latency_ns: usize,
  pub media_addr: SocketAddr,
}

/// Upper bounds for values a remote transmitter advertises. They size per-flow
/// allocations and latency arithmetic on the receiving side.
pub const MAX_ADVERTISED_CHANNELS_PER_FLOW: usize = 64;
pub const MAX_ADVERTISED_LATENCY_NS: usize = 1_000_000_000;

/// Rejects an advertised media format the receiver cannot handle. Values come
/// from mDNS TXT records of any device on the network.
pub fn check_advertised_format(
  channels_per_flow: usize,
  bits_per_sample: u32,
  min_rx_latency_ns: usize,
) -> Result<(), io::Error> {
  let bad = |what: String| {
    error!("ignoring advertisement: {what}");
    Err(io::Error::new(io::ErrorKind::InvalidData, what))
  };
  if !matches!(bits_per_sample, 16 | 24 | 32) {
    return bad(format!("unsupported encoding {bits_per_sample}"));
  }
  if channels_per_flow == 0 || channels_per_flow > MAX_ADVERTISED_CHANNELS_PER_FLOW {
    return bad(format!("nchan={channels_per_flow}"));
  }
  if min_rx_latency_ns > MAX_ADVERTISED_LATENCY_NS {
    return bad(format!("latency_ns={min_rx_latency_ns}"));
  }
  Ok(())
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

pub fn self_origin_from_self_info(self_info: &DeviceInfo) -> Vec<u8> {
  (hex::encode(self_info.factory_device_id) + "," + &self_info.process_id.to_string()).into()
}

pub struct MdnsClient {
  listen_ip: Ipv4Addr,
  self_origin: Vec<u8>,
  self_info: Arc<DeviceInfo>,
}

impl MdnsClient {
  pub fn new(self_info: Arc<DeviceInfo>) -> Self {
    Self {
      listen_ip: self_info.ip_address,
      self_origin: self_origin_from_self_info(&self_info),
      self_info,
    }
  }
  async fn do_single_query(
    &self,
    types: &[RecordType],
    fqdn: &Name,
    timeout: Duration,
  ) -> Result<DnsResponse, Box<dyn Error>> {
    DiscoveryBuilder::new()
      .interface_v4(TargetInterface::Specific(self.listen_ip))
      .loopback()
      .build(IpVersion::V4)
      .map_err(|e| Box::new(e))?
      .single_query(types, fqdn, timeout, |origin_addr, response| {
        let is_local = match origin_addr.ip() {
          std::net::IpAddr::V4(addr) => addr.is_loopback() || addr == self.listen_ip,
          _ => false,
        };
        if is_local {
          let response_origin_fqdn = Name::from_labels(["_inferno-response-origin", "local"]).unwrap();
          for rr in response.additionals() {
            if rr.name() == &response_origin_fqdn {
              if let Some(data) = rr.data() {
                if let Some(txt) = data.as_txt() {
                  if let Some(data) = txt.txt_data().get(0) {
                    if data.eq_ignore_ascii_case(&self.self_origin) {
                      debug!("got answer from self, ignoring this response");
                      return false;
                    } else {
                      debug!("got local answer from other instance, considering");
                      return true;
                    }
                  }
                }
              }
            }
          }
          true
        } else {
          true
        }
      })
      .await
      .map_err(|e| Box::new(e).into())
  }
  async fn a_record_exists_single_check(
    &self,
    fqdn_parts: &[&str],
    timeout: Duration,
  ) -> Result<bool, Box<dyn Error>> {
    let fqdn = Name::from_labels(fqdn_parts.iter().map(|&s| s.as_bytes())).map_err(|e| Box::new(e))?;
    match self.do_single_query(&[RecordType::A], &fqdn, timeout).await {
      Ok(response) => {
        for record in response.answers() {
          if record.name().to_lowercase() == fqdn.to_lowercase() {
            return Ok(true);
          }
        }
        // a response that answers some other name is not this record
        Ok(false)
      }
      Err(_e) => Ok(false), // TODO: sometimes error may be other than timeout, handle that
    }
  }
  pub async fn a_record_exists(&self, fqdn_parts: &[&str]) -> Result<bool, Box<dyn Error>> {
    debug!("checking if A record exists: {fqdn_parts:?}");
    for _ in 0..3 {
      let r = self.a_record_exists_single_check(fqdn_parts, Duration::from_millis(400)).await?;
      if r {
        return Ok(r);
      };
    }
    Ok(false)
  }
  pub async fn is_multicast_ip_already_used(&self, addr: Ipv4Addr) -> Result<bool, Box<dyn Error>> {
    let octets = addr.octets();
    let name = [
      &octets[3].to_string(),
      &octets[2].to_string(),
      &octets[1].to_string(),
      &octets[0].to_string(),
      "in-addr",
      "local",
    ];
    self.a_record_exists(&name).await
  }

  pub async fn query(&self, fqdn_parts: &[&str]) -> Result<AdvertisedService, Box<dyn Error>> {
    debug!("resolving {fqdn_parts:?}");
    let fqdn = Name::from_labels(fqdn_parts.iter().map(|&s| s.as_bytes())).map_err(|e| Box::new(e))?;
    let response =
      self.do_single_query(&[RecordType::SRV, RecordType::TXT], &fqdn, Duration::from_secs(3)).await?;
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
      // if at this point we haven't returned from this method,
      // it means that additional A record wasn't contained in the response
      let a_response = self.do_single_query(&[RecordType::A], &name, Duration::from_secs(3)).await?;
      for record in a_response.answers() {
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

  fn parse_int_from_dict(dict: &BTreeMap<String, String>, key: &str) -> Result<usize, Box<dyn Error>> {
    match dict.get(key) {
      Some(s) => {
        let result =
          if s.starts_with("0x") { usize::from_str_radix(&s[2..], 16) } else { s.parse::<usize>() };
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
  }

  pub async fn query_chan(
    &self,
    tx_hostname: &str,
    tx_channel_name: &str,
  ) -> Result<AdvertisedChannel, Box<dyn Error>> {
    let full_name = format!("{}@{}", tx_channel_name, tx_hostname);
    let fqdn = [&full_name, "_netaudio-chan", "_udp", "local"];
    let result = self.query(&fqdn).await?;
    let parse_int =
      |key| -> Result<usize, Box<dyn Error>> { Self::parse_int_from_dict(&result.properties, key) };
    let mut multicast = None;
    for (key, value) in &result.properties {
      if key.starts_with("b.") {
        multicast = Some(PointerToMulticast {
          tx_hostname: tx_hostname.to_owned(),
          bundle_id: match key[2..].parse::<usize>() {
            Ok(v) => v,
            Err(_e) => {
              error!("Unable to parse multicast bundle key {key}");
              break;
            }
          },
          channel_in_bundle: match value.parse::<usize>() {
            Ok(v) if v > 0 => v - 1,
            _ => {
              error!("Unable to parse multicast bundle value {value}");
              break;
            }
          },
        });
        break;
      }
    }
    let (fpp1, fpp2) = result
      .properties
      .get("fpp")
      .ok_or(Box::new(io::Error::from(io::ErrorKind::NotFound)))?
      .split_once(",")
      .ok_or(Box::new(io::Error::from(io::ErrorKind::InvalidData)))?;
    let channel = AdvertisedChannel {
      addr: result.addr,
      tx_channels_per_flow: parse_int("nchan")?,
      tx_channel_id: parse_int("id")? as u16,
      bits_per_sample: parse_int("enc").or_else(|_| parse_int("en"))? as u32,
      dbcp1: parse_int("dbcp1")? as u16,
      fpp_min: fpp2.parse()?,
      fpp_max: fpp1.parse()?,
      min_rx_latency_ns: parse_int("latency_ns")?,
      multicast,
    };
    check_advertised_format(
      channel.tx_channels_per_flow,
      channel.bits_per_sample,
      channel.min_rx_latency_ns,
    )?;
    if channel.fpp_min == 0 || channel.fpp_max < channel.fpp_min {
      error!("ignoring advertisement of {full_name}: fpp={fpp1},{fpp2}");
      return Err(Box::new(io::Error::from(io::ErrorKind::InvalidData)));
    }
    return Ok(channel);
  }

  pub async fn query_bund(&self, full_name: &str) -> Result<AdvertisedBundle, Box<dyn Error>> {
    let fqdn = [full_name, "_netaudio-bund", "_udp", "local"];
    let result = self.query(&fqdn).await?;
    let parse_int =
      |key| -> Result<usize, Box<dyn Error>> { Self::parse_int_from_dict(&result.properties, key) };
    let ip =
      result.properties.get("a.0").ok_or(Box::new(io::Error::from(io::ErrorKind::NotFound)))?.parse()?;
    let port =
      result.properties.get("p.0").ok_or(Box::new(io::Error::from(io::ErrorKind::NotFound)))?.parse()?;
    let media_addr = SocketAddr::V4(SocketAddrV4::new(ip, port));
    let bundle = AdvertisedBundle {
      tx_channels_per_flow: parse_int("nchan")?,
      tx_bundle_id: parse_int("id")? as u16,
      bits_per_sample: parse_int("enc").or_else(|_| parse_int("en"))? as u32,
      fpp: parse_int("fpp")? as u16,
      min_rx_latency_ns: parse_int("latency_ns")?,
      media_addr,
    };
    check_advertised_format(
      bundle.tx_channels_per_flow,
      bundle.bits_per_sample,
      bundle.min_rx_latency_ns,
    )?;
    if bundle.fpp == 0 {
      error!("ignoring advertisement of {full_name}: fpp=0");
      return Err(Box::new(io::Error::from(io::ErrorKind::InvalidData)));
    }
    return Ok(bundle);
  }
}

#[cfg(test)]
mod advert_tests {
  use super::*;

  #[test]
  fn advertised_formats() {
    assert!(check_advertised_format(8, 24, 1_000_000).is_ok());
    assert!(check_advertised_format(1, 16, 0).is_ok());
    assert!(
      check_advertised_format(MAX_ADVERTISED_CHANNELS_PER_FLOW, 32, MAX_ADVERTISED_LATENCY_NS).is_ok()
    );
    // each of these reached a panic, a division by zero or an overflow on the receive side
    assert!(check_advertised_format(0, 24, 0).is_err());
    assert!(check_advertised_format(8, 0, 0).is_err());
    assert!(check_advertised_format(8, 7, 0).is_err());
    assert!(check_advertised_format(8, 40, 0).is_err());
    assert!(check_advertised_format(MAX_ADVERTISED_CHANNELS_PER_FLOW + 1, 24, 0).is_err());
    assert!(check_advertised_format(8, 24, usize::MAX).is_err());
  }
}

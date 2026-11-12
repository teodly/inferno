use std::{
  collections::BTreeMap,
  env,
  net::{IpAddr, Ipv4Addr},
  path::PathBuf,
  sync::{Arc, RwLock},
};

use netdev::mac::MacAddr;

use crate::device_info::{Channel, DeviceInfo};
use crate::protocol::flows_control::PORT as FLOWS_CONTROL_PORT;
use crate::protocol::mcast::INFO_REQUEST_PORT;
use crate::protocol::proto_arc::PORT as ARC_PORT;
use crate::protocol::proto_cmc::PORT as CMC_PORT;

fn create_self_info(
  app_name: &str,
  short_app_name: &str,
  my_ip: Option<Ipv4Addr>,
  settings: &BTreeMap<String, String>,
) -> DeviceInfo {
  // TODO: change expect to non-fatal errors, with current approach an app using ALSA plugin may be crashed for a bening reason

  let interfaces = netdev::get_interfaces();
  let my_ipv4 = my_ip
    .or_else(|| {
      settings.get("BIND_IP").map(|ipstr| {
        ipstr.parse().unwrap_or_else(|_| {
          interfaces
            .iter()
            .find(|iface| &iface.name == ipstr)
            .and_then(|iface| iface.ipv4.get(0).map(|n| n.addr()))
            .unwrap_or_else(|| {
              log::warn!("invalid BIND_IP {ipstr:?}; falling back to auto-detected IP");
              // fall back to unspecified; caller will auto-detect below
              Ipv4Addr::new(0, 0, 0, 0)
            })
        })
      })
    })
    .unwrap_or_else(|| match local_ip_address::local_ip().expect("unknown local IP, cannot continue") {
      IpAddr::V4(a) => a,
      other => panic!("got local IP which is not IPv4: {other:?}"),
    });

  let process_id: u16 =
    settings.get("PROCESS_ID").map(|s| s.parse().expect("PROCESS_ID must be u16")).unwrap_or(0);

  let mut devid = [0u8; 8];
  settings
    .get("DEVICE_ID")
    .map(|idstr| {
      if hex::decode_to_slice(idstr, &mut devid).is_err() {
        log::warn!("invalid DEVICE_ID {idstr:?}; using generated id");
      }
    })
    .unwrap_or_else(|| {
      devid[2..6].copy_from_slice(&my_ipv4.octets());
      devid[6..8].copy_from_slice(&process_id.to_be_bytes());
    });

  // TODO make hostname and sample rate configurable from DC
  let friendly_hostname = settings
    .get("NAME")
    .map(|s| truncate_utf8(s, 31).to_owned())
    .unwrap_or_else(|| {
      format!(
        "{} {}",
        truncate_utf8(app_name, 22),
        hex::encode(&my_ipv4.octets())
      )
    });
  let short_app_name = truncate_utf8(short_app_name, 14);

  let sample_rate = settings
    .get("SAMPLE_RATE")
    .map(|s| s.parse().expect("invalid SAMPLE_RATE, must be integer"))
    .unwrap_or(48000);

  let mut netmask = Ipv4Addr::new(0, 0, 0, 0);
  let mut gateway = Ipv4Addr::new(0, 0, 0, 0);
  let mut mac_address = MacAddr::zero();
  let mut speed = 0;
  for iface in interfaces {
    let mut our_iface = false;
    for network in iface.ipv4 {
      if network.addr() == my_ipv4 {
        netmask = network.netmask();
        our_iface = true;
        break;
      }
    }
    if our_iface {
      speed =
        [iface.transmit_speed.unwrap_or(0), iface.receive_speed.unwrap_or(0)].iter().max().unwrap_or(&0)
          / 1_000_000;
      if let Some(gws) = iface.gateway {
        for gw in gws.ipv4 {
          if (gw.to_bits() & netmask.to_bits()) == (my_ipv4.to_bits() & netmask.to_bits()) {
            gateway = gw;
            break;
          }
        }
      }
      if let Some(mac) = iface.mac_addr {
        mac_address = mac;
      }
      break;
    }
  }

  let latency_ns = settings
    .get("RX_LATENCY_NS")
    .map(|s| s.parse().expect("invalid RX_LATENCY_NS, must be integer"))
    .unwrap_or(10_000_000);

  let mut result = DeviceInfo {
    ip_address: my_ipv4,
    netmask,
    gateway,
    mac_address,
    link_speed: speed.clamp(0, 10000).try_into().unwrap(),

    board_name: "Inferno-AoIP".to_owned(),
    manufacturer: "Inferno-AoIP".to_owned(),
    model_name: app_name.to_owned(),
    factory_device_id: devid,
    process_id,
    vendor_string: "Audinate Dante-compatible".to_owned(),
    factory_hostname: format!("{short_app_name}-{}", hex::encode(devid)),
    friendly_hostname,
    model_number: "_000000000000000b".to_owned(),
    rx_channels: vec![],
    tx_channels: vec![],
    bits_per_sample: 24, // TODO make it configurable
    pcm_type: 0xe,
    latency_ns,
    sample_rate,

    arc_port: ARC_PORT,
    cmc_port: CMC_PORT,
    flows_control_port: FLOWS_CONTROL_PORT,
    info_request_port: INFO_REQUEST_PORT,
    product_version: settings.get("PRODUCT_VERSION").and_then(|v| parse_product_version(v)),
    name_request_path: settings.get("NAME_REQUEST_PATH").map(Into::into),
    latency_request_path: settings.get("LATENCY_REQUEST_PATH").map(Into::into),
    sample_rate_request_path: settings.get("SAMPLE_RATE_REQUEST_PATH").map(Into::into),
    announced_sample_rate: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(sample_rate.try_into().unwrap_or(48000))),
    announced_latency_ns: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(latency_ns.try_into().unwrap_or(u32::MAX))),
    state_dir: settings.get("STATE_DIR").map(Into::into),
  };

  if let Some(altport) = settings.get("ALT_PORT").map(|s| s.parse::<u16>().expect("ALT_PORT must be u16")) {
    // The three following ports are used too: 65533..=65535 used to
    // overflow (a panic in debug builds, a wrap to ports 0.. in release).
    assert!(altport <= u16::MAX - 3, "ALT_PORT must be at most {}", u16::MAX - 3);
    result.arc_port = altport;
    result.cmc_port = altport + 1;
    result.flows_control_port = altport + 2;
    result.info_request_port = altport + 3;
  }

  result
}

/// The longest prefix of `s` that is at most `max` bytes and ends on a
/// character boundary. Slicing `&s[0..max]` panicked when byte `max` fell
/// inside a multi-byte character (a non-ASCII NAME, say).
fn truncate_utf8(s: &str, max: usize) -> &str {
  if s.len() <= max {
    return s;
  }
  let mut end = max;
  while !s.is_char_boundary(end) {
    end -= 1;
  }
  &s[..end]
}

/// "major.minor.patch" (patch optional) as announced product version
/// fields; anything else is ignored with a warning.
pub fn parse_product_version(v: &str) -> Option<(u8, u8, u16)> {
  let mut parts = v.trim().split('.');
  let parsed = (|| {
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
      Some(p) => p.parse().ok()?,
      None => 0,
    };
    parts.next().is_none().then_some((major, minor, patch))
  })();
  if parsed.is_none() {
    log::warn!("ignoring PRODUCT_VERSION {v:?}: expected major.minor[.patch]");
  }
  parsed
}

#[cfg(test)]
mod product_version_tests {
  use super::parse_product_version;

  #[test]
  fn parses_dotted_versions() {
    assert_eq!(parse_product_version("1.20.0"), Some((1, 20, 0)));
    assert_eq!(parse_product_version(" 2.3 "), Some((2, 3, 0)));
    assert_eq!(parse_product_version("1.1.300"), Some((1, 1, 300)));
    for bad in ["", "1", "1.x", "1.2.3.4", "256.0.0", "v1.2.3"] {
      assert_eq!(parse_product_version(bad), None, "{bad:?}");
    }
  }
}

#[derive(Clone)] // TODO: this shouldn't need to be clonable, fix the ALSA plugin
pub struct Settings {
  pub self_info: DeviceInfo,
  pub tx_latency_ns: u32,
  /// MAX_RX_LATENCY_NS: a subscription whose latency (the larger of ours and
  /// the transmitter's advertised) exceeds this is refused with the "channel latency" status.
  pub max_rx_latency_ns: u32,
  pub clock_path: Option<PathBuf>,
  pub use_safe_clock: bool,
  pub tx_source_bit_depth: u8,
  /// FIXED_LAST_CHANNEL_NAME: the last RX and the last TX channel carry this
  /// name and cannot be renamed (an application's dedicated channel, e.g. a
  /// timecode channel). None leaves every channel renamable.
  pub fixed_last_channel_name: Option<String>,
}

impl Settings {
  pub fn new(
    app_name: &str,
    short_app_name: &str,
    my_ip: Option<Ipv4Addr>,
    config: &BTreeMap<String, String>,
  ) -> Self {
    // convert all settings keys to upper case:
    let mut config: BTreeMap<String, String> =
      config.clone().into_iter().map(|(k, v)| (k.to_ascii_uppercase(), v)).collect();

    // add settings from env vars if not already set:
    env::vars().for_each(|(env_key, env_value)| {
      if let Some(key) = env_key.strip_prefix("INFERNO_") {
        let key = key.to_ascii_uppercase();
        config.entry(key).or_insert(env_value);
      }
    });
    let self_info = create_self_info(app_name, short_app_name, my_ip, &config);

    let use_safe_clock = config
      .get("USE_SAFE_CLOCK")
      .map(|s| s.parse().expect("invalid USE_SAFE_CLOCK, must be boolean"))
      .unwrap_or(false);
    let tx_source_bit_depth = config
      .get("TX_SOURCE_BIT_DEPTH")
      .map(|s| s.parse::<u8>().unwrap_or(32))
      .unwrap_or(32);
    let tx_source_bit_depth = if matches!(tx_source_bit_depth, 16 | 24 | 32) {
      tx_source_bit_depth
    } else {
      log::warn!("invalid TX_SOURCE_BIT_DEPTH {tx_source_bit_depth}; using 32");
      32
    };

    let mut result = Self {
      self_info,
      tx_latency_ns: config
        .get("TX_LATENCY_NS")
        .map(|p| p.parse().expect("invalid TX_LATENCY_NS, must be integer"))
        .unwrap_or(10_000_000),
      max_rx_latency_ns: config
        .get("MAX_RX_LATENCY_NS")
        .map(|p| p.parse().expect("invalid MAX_RX_LATENCY_NS, must be integer"))
        .unwrap_or(crate::mdns_client::MAX_ADVERTISED_LATENCY_NS as u32),
      clock_path: config.get("CLOCK_PATH").map(|p| p.try_into().unwrap()),
      use_safe_clock,
      tx_source_bit_depth,
      fixed_last_channel_name: config
        .get("FIXED_LAST_CHANNEL_NAME")
        .filter(|n| {
          let ok = super::saved_settings::is_valid_channel_name(n);
          if !ok {
            log::warn!("ignoring FIXED_LAST_CHANNEL_NAME {n:?}: not a valid channel name");
          }
          ok
        })
        .cloned(),
    };

    // the following should be harmless, as the application still has the chance to overwrite it
    let rx_count =
      config.get("RX_CHANNELS").map(|s| s.parse().expect("number of channels must be u16")).unwrap_or(2);
    result.make_rx_channels(rx_count);
    let tx_count =
      config.get("TX_CHANNELS").map(|s| s.parse().expect("number of channels must be u16")).unwrap_or(2);
    result.make_tx_channels(tx_count);

    result
  }
  pub fn make_rx_channels(&mut self, count: usize) {
    self.self_info.rx_channels = make_channels("RX", count, self.fixed_last_channel_name.as_deref());
  }
  pub fn make_tx_channels(&mut self, count: usize) {
    self.self_info.tx_channels = make_channels("TX", count, self.fixed_last_channel_name.as_deref());
  }
}

/// Channels 1..=count named "<prefix> <n>"; with fixed_last, the last one is
/// named that instead and marked fixed.
fn make_channels(prefix: &str, count: usize, fixed_last: Option<&str>) -> Vec<Channel> {
  (1..=count)
    .map(|id| {
      let fixed = fixed_last.filter(|_| id == count);
      Channel {
        factory_name: format!("{id:02}"),
        friendly_name: Arc::new(RwLock::new(fixed.map_or_else(|| format!("{prefix} {id}"), str::to_owned))),
        fixed_name: fixed.is_some(),
      }
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::{make_channels, truncate_utf8};

  #[test]
  fn fixed_last_channel_name() {
    let chans = make_channels("RX", 3, Some("TIMECODE"));
    let names: Vec<String> = chans.iter().map(|c| c.friendly_name.read().unwrap().clone()).collect();
    assert_eq!(names, ["RX 1", "RX 2", "TIMECODE"]);
    assert_eq!(chans.iter().map(|c| c.fixed_name).collect::<Vec<_>>(), [false, false, true]);
    assert!(make_channels("TX", 2, None).iter().all(|c| !c.fixed_name));
    assert!(make_channels("TX", 0, Some("TIMECODE")).is_empty());
  }

  #[test]
  fn truncate_utf8_never_splits_a_character() {
    assert_eq!(truncate_utf8("short", 31), "short");
    assert_eq!(truncate_utf8("abcdef", 3), "abc");
    // 'é' is two bytes: byte 3 falls inside the second one
    assert_eq!(truncate_utf8("aééb", 4), "aé");
    assert_eq!(truncate_utf8("aééb", 5), "aéé");
    let name = "Ünïcödé recorder name that is long";
    let cut = truncate_utf8(name, 31);
    assert!(cut.len() <= 31 && name.starts_with(cut));
  }
}

#[cfg(test)]
mod robustness_tests {
  use super::*;

  #[test]
  fn truncate_utf8_max_zero() {
    assert_eq!(truncate_utf8("abc", 0), "");
  }

  #[test]
  fn truncate_utf8_empty_input() {
    assert_eq!(truncate_utf8("", 5), "");
  }

  #[test]
  fn parse_product_version_rejects_garbage() {
    assert_eq!(parse_product_version("1.2.3.4"), None);
    assert_eq!(parse_product_version("abc"), None);
    assert_eq!(parse_product_version(""), None);
  }

  #[test]
  fn parse_product_version_accepts_two_parts() {
    assert_eq!(parse_product_version("1.2"), Some((1, 2, 0)));
  }
}

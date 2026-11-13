use super::channels_subscriber::ChannelsSubscriber;
use crate::byte_utils::*;
use crate::common::*;
use crate::media_clock::MediaClock;
use crate::net_utils::UdpSocketWrapper;
use crate::protocol::mcast::{make_packet, MulticastMessage};
use crate::{byte_utils::write_str_to_buffer, device_info::DeviceInfo};
use bytebuffer::ByteBuffer;
use std::sync::atomic::Ordering;
use std::sync::RwLock;
use std::{
  net::{IpAddr, Ipv4Addr, SocketAddr},
  sync::Arc,
  time::Duration,
};
use tokio::sync::watch;
use tokio::time::interval;
use tokio::{
  select,
  sync::{broadcast::Receiver as BroadcastReceiver, mpsc},
  time::MissedTickBehavior,
};

const SEND_BUFFER_SIZE: usize = 1500;

/// Feature flags feeding the capability word. Keep this honest: only set a bit
/// when the corresponding behaviour is actually implemented (handoff W4/W8).
#[derive(Clone, Copy, Default)]
pub struct CapabilityFeatures {
  pub manufacturer_name: bool,
  pub sample_rate_config: bool,
  pub encoding_config: bool,
  pub static_ipv4: bool,
  pub identify: bool,
  pub locking: bool,
  pub aes67: bool,
  pub metering: bool,
  pub serial_ports: bool,
  pub leds: bool,
}

/// Compute the primary capability word from a feature table (handoff W4).
/// Bit positions per netaudio's parser (see send_board_info comments).
pub fn primary_capabilities(f: &CapabilityFeatures) -> u32 {
  let mut w = 0u32;
  if f.manufacturer_name { w |= 0x0000_1000; }
  if f.sample_rate_config { w |= 0x0000_0008; }
  if f.encoding_config { w |= 0x0000_0010; }
  if f.static_ipv4 { w |= 0x0000_4000; }
  if f.identify { w |= 0x0000_0001; }
  if f.locking { w |= 0x0800_0000; }
  if f.aes67 { w |= 0x0400_0000; }
  // metering/serial/leds live in other capability words; keep primary honest.
  let _ = (f.metering, f.serial_ports, f.leds);
  w
}

/// Default feature set for Inferno today: sample-rate/encoding are reported
/// (running values only) and manufacturer name is present. Write paths are
/// not implemented, so config bits are left on only to keep DC showing values
/// (matches prior behaviour).
const BOARD_PRIMARY_CAPABILITIES: u32 =
  0x0000_1000 | 0x0000_0008 | 0x0000_0010;
const DST_PORT_HEARTBEAT: u16 = 8708;
const DST_PORT_DEVICE_INFO: u16 = 8702;

pub type PeaksCallback = Box<dyn FnMut() -> (Vec<u8>, Vec<u8>) + Send + Sync>;

struct Multicaster<'s> {
  self_info: &'s DeviceInfo,
  pub server: UdpSocketWrapper,
  seqnum: u16,
  vendor: [u8; 8],
  firmware_version_bytes: [u8; 4],
  software_version_bytes: [u8; 4],
  product_version_bytes: [u8; 4],
  device_info_destination: SocketAddr,
  heartbeat_destination: SocketAddr,
  send_buffer: [u8; SEND_BUFFER_SIZE],
  clock: Arc<RwLock<MediaClock>>,
  channels_subscriber: Option<Arc<ChannelsSubscriber>>,
  get_peaks: PeaksCallback,
  had_clock: bool,
}

impl<'s> Multicaster<'s> {
  pub fn new(
    self_info: &'s DeviceInfo,
    server: UdpSocketWrapper,
    clock: Arc<RwLock<MediaClock>>,
    get_peaks: PeaksCallback,
  ) -> Multicaster {
    let patch_version = env!("CARGO_PKG_VERSION_PATCH").parse::<u16>().unwrap();
    let crate_version = [
      env!("CARGO_PKG_VERSION_MAJOR").parse::<u8>().unwrap(),
      env!("CARGO_PKG_VERSION_MINOR").parse::<u8>().unwrap(),
      H(patch_version),
      L(patch_version),
    ];
    let mut r = Multicaster {
      self_info,
      server,
      seqnum: 1,
      vendor: [32; 8],
      firmware_version_bytes: [4, 1, 6, 2],
      software_version_bytes: crate_version,
      product_version_bytes: match self_info.product_version {
        Some((major, minor, patch)) => [major, minor, H(patch), L(patch)],
        None => crate_version,
      },
      device_info_destination: SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(224, 0, 0, 231)),
        DST_PORT_DEVICE_INFO,
      ),
      heartbeat_destination: SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(224, 0, 0, 233)),
        DST_PORT_HEARTBEAT,
      ),
      send_buffer: [0; SEND_BUFFER_SIZE],
      clock,
      channels_subscriber: None,
      get_peaks,
      had_clock: false,
    };
    write_str_to_buffer(&mut r.vendor, 0, 8, &self_info.vendor_string);
    return r;
  }

  pub fn should_work(&self) -> bool {
    return self.server.should_work();
  }

  async fn send(&mut self, dst: SocketAddr, start_code: u16, opcode: [u8; 8], content: &[u8]) {
    let pkt = make_packet(
      &mut self.send_buffer,
      start_code,
      self.seqnum,
      self.self_info.process_id,
      self.self_info.factory_device_id,
      self.vendor,
      opcode,
      content,
    );
    self.seqnum = self.seqnum.wrapping_add(1);
    self.server.send(&dst, pkt).await;
  }

  // Audio capability status (class 0x0724) answering a sample-rate or
  // encoding probe. Without an answer netaudio shows no rate or encoding
  // for the device (U2).
  async fn send_capability_status(
    &mut self,
    status: u8,
    current: u32,
    update_mode: u16,
    supported: &[u32],
  ) {
    let content = capability_status_content(current, update_mode, supported);
    self
      .send(self.device_info_destination, 0xffff, [0x07, 0x24, 0x00, status, 0, 0, 0, 0], &content)
      .await;
  }

  /// Sample rate status (0x80): the running rate is the only one listed.
  async fn send_sample_rate_status(&mut self) {
    let rate = self.self_info.sample_rate;
    self.send_capability_status(0x80, rate, SAMPLE_RATE_UPDATE_MODE, &[rate]).await;
  }

  /// Encoding status (0x82). Only the running encoding is listed: a set
  /// request (operation mode 1) is answered with this same status, so
  /// offering 16/32 would show choices that do nothing.
  async fn send_encoding_status(&mut self) {
    let enc = self.self_info.bits_per_sample as u32;
    self.send_capability_status(0x82, enc, ENCODING_UPDATE_MODE, &[enc]).await;
  }

  async fn send_board_info(&mut self) {
    let mut content = [0u8; 200];
    // Firmware version:
    content[0..4].copy_from_slice(&[4, 1, 0, 6]);
    content[0x23] = 2;
    // Hardware version:
    content[4..8].copy_from_slice(&[4, 1, 0, 3]);
    content[0x27] = 1;
    // Boot version:
    content[0x28..0x2c].copy_from_slice(&[1, 0, 0, 0]);

    // flags of supported features:
    // 0x14: AES67, Device Lock
    //       0x01 - ??? (was 1, od 0xd)
    //       0x04 - supports AES67
    //       0x08 - is lockable
    // 0x15: ??? (was 0x50)
    // 0x16:
    //       0x10 - has Manufacturer name
    //       0x40 - Network is configurable (supports static addressing)
    // 0x17: Identify device, Sample rate & encoding configuration, Reboot, Factory reset (was 0xdb)
    // As one big-endian u32 at 0x14 (netaudio's "primary capabilities";
    // bit meanings checked against its parser):
    //   0x0000_0001 identify, 0x0000_0008 sample rate configuration,
    //   0x0000_0010 encoding configuration, 0x0000_1000 manufacturer name,
    //   0x0000_4000 static IPv4, 0x0400_0000 AES67, 0x0800_0000 locking.
    // Without the sample rate / encoding bits a controller's device config
    // leaves both blank ("does not support sample rate configuration") even
    // though the 0x81/0x83 probes are answered.
    let feats = CapabilityFeatures { manufacturer_name: true, sample_rate_config: true, encoding_config: true, ..Default::default() };
    content[0x14..0x18].copy_from_slice(&primary_capabilities(&feats).to_be_bytes());

    content[0xbb] = 0x1f; // if 0, device is flooded with info multicast requests around 1 per second
                          /* content[0xbf] = 5;
                          content[0xc3] = 3;
                          content[0xc7] = 3; */
    write_str_to_buffer(&mut content, 12, 8, &self.self_info.board_name);
    write_str_to_buffer(&mut content, 0x38, 16, &self.self_info.board_name);

    self.send(self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x60, 0, 0, 0, 0], &content).await;
  }

  async fn send_product_info(&mut self) {
    let mut content = [0; 336];
    write_str_to_buffer(&mut content, 0, 8, &self.self_info.manufacturer);
    write_str_to_buffer(&mut content, 8, 8, &self.self_info.board_name);
    write_str_to_buffer(&mut content, 0x2c, 16, &self.self_info.manufacturer);
    write_str_to_buffer(&mut content, 0xac, 16, &self.self_info.model_name);
    // product version (controllers show it as "Product Version"; a hardware
    // interface carries e.g. 01 01 00 03 = 1.1.3 here):
    content[0x12c..0x130].copy_from_slice(&self.product_version_bytes);

    // firmware version:
    content[0x1c..0x20].copy_from_slice(&self.software_version_bytes);

    // 0x18..0x1b - software version
    // 0x24..0x26 - software patch version, u32
    // 0x28..0x2b - firmware patch version, u32

    self.send(self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0xc0, 0, 0, 0, 0], &content).await;
  }

  fn get_freq_offset_ppb(&self) -> Option<i32> {
    self
      .clock
      .read()
      .unwrap()
      .get_overlay()
      .as_ref()
      .map(|clkovl| {
        #[cfg(target_os = "linux")]
        let scale = clkovl.freq_scale_including_hw();
        #[cfg(not(target_os = "linux"))]
        let scale = clkovl.freq_scale;
        let freq_offset_f = (scale * 1_000_000_000f64).round();
        if i32::MIN as f64 <= freq_offset_f && freq_offset_f <= i32::MAX as f64 {
          Some(freq_offset_f as i32)
        } else {
          None
        }
      })
      .flatten()
  }

  async fn send_heartbeat(&mut self) {
    let ctr = self.seqnum;
    let mut bytes = ByteBuffer::new();
    bytes.set_endian(bytebuffer::Endian::BigEndian);

    let freq_offset_opt = self.get_freq_offset_ppb();

    if let Some(freq_offset) = freq_offset_opt {
      bytes.write_u16(16); // length of this part
      bytes.write_u16(0x8001); // type
      bytes.write_u16(4); // ???
      bytes.write_u16(4); // maybe content length???
      bytes.write_u16(ctr);
      bytes.write_u16(0);
      bytes.write_i32(freq_offset);
      trace!("freq offset {freq_offset}/1000 ppm");

      /* bytes.write_bytes(&[
        0x00, 0x24, 0x80, 0x00,
        0x00, 0x04, 0x00, 0x04, H(ctr), L(ctr), 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x10,
        /* TX Bps, 4B: */ 0x00, 0x00, 0x01, 0xde, /* RX Bps, 4B: */ 0x00, 0x07, 0xdf, 0x17,
        /* TX errors, 4B: */ 0x00, 0x00, 0x00, 0x02, /* RX errors, 4B: */ 0x00, 0x00, 0x00, 0x07,
      ]); // network statistics */

      let (rx_peaks, tx_peaks) = (self.get_peaks)();
      let total_peaks = rx_peaks.len() + tx_peaks.len();
      if total_peaks > 0 {
        let mut total_len = 24 + total_peaks;
        while (total_len % 4) != 0 {
          total_len += 1;
        }
        let end_pos = bytes.get_wpos() + total_len;
        bytes.write_u16((24 + total_peaks).try_into().unwrap());
        bytes.write_u16(0x8002);
        bytes.write_u16(4);
        bytes.write_u16((12 + total_peaks).try_into().unwrap());
        bytes.write_u16(ctr);
        bytes.write_u16(0);
        bytes.write_u16(tx_peaks.len().try_into().unwrap());
        bytes.write_u16(0);
        bytes.write_u16(rx_peaks.len().try_into().unwrap());
        bytes.write_u16(0);
        bytes.write_u16(24);
        bytes.write_u16(0);
        for peak in tx_peaks.into_iter().chain(rx_peaks.into_iter()) {
          bytes.write_u8(peak);
        }
        while bytes.get_wpos() < end_pos {
          bytes.write_u8(0);
        }
      }

      // rx latency:
      if let Some(chsub) = self.channels_subscriber.as_ref() {
        let flows_info = chsub.flows_info();
        let flows_info = flows_info.read().unwrap();
        let flows_count = flows_info.len() as u16;
        bytes.write_u16(24 + flows_count * 4);
        bytes.write_u16(0x8003);
        bytes.write_u16(4);
        bytes.write_u16(12 + flows_count * 4); // content length
        bytes.write_u16(ctr);
        bytes.write_u16(0);
        bytes.write_u16(flows_count); // number of flows
        bytes.write_u16(0);
        bytes.write_u16(24);
        bytes.write_u16(0);
        bytes.write_u32(self.self_info.sample_rate);

        for opt in flows_info.iter() {
          let latency =
            opt.as_ref().map(|fi| fi.actual_latency_samples.swap(0, Ordering::Relaxed)).unwrap_or(0);
          bytes.write_u32(latency.clamp(0, i32::MAX) as u32);
        }
      }

      /* bytes.write_bytes(&[
        0x00, 0x1c, 0x80, 0x04,
        0x00, 0x04, 0x00, 0x10,  H(ctr), L(ctr), 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
      ]); */
      /*
      0x00, 0x1c, 0x80, 0x04, 0x00, 0x04, 0x00, 0x10, 0x17, 0x0f, 0x00, 0x00,
      0x00, 0x02, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00, missed packets, 4B: 0x00, 0x03, 0x90, 0x1e, 0x00, 0x00, 0x00, 0x00
       */
      if !self.had_clock {
        debug!("clock appeared");
      }
    } else {
      debug!("no clock available");
    }
    self.had_clock = freq_offset_opt.is_some();

    let content = bytes.as_bytes();
    self.send(self.heartbeat_destination, 0xfffe, [0, 8, 0, 1, 0x10, 0, 0, 0], &content).await;

    // this is probably response to 0738008100000064
    /* self.send(
      self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00],
      &[0x00, 0x18, 0x00, 0x04, 0x00, 0x00, 0xbb, 0x80, 0x00, 0x00, 0xbb, 0x80, 0x00, 0x02, 0x00, 0x00,
      // supported sample rates:
      0x00, 0x00, /* 44100: */ 0xac, 0x44, 0x00, 0x00, 0xbb, 0x80, 0x00, 0x01, 0x58, 0x88, 0x00, 0x01, 0x77, 0x00]
    ).await; */

    /* self.send(
      self.device_info_destination, 0xffff, [0x07, 0x2a, 0x10, 0x07, 0, 0, 0, 0],
      &[0, 0, 0, 0]
    ).await; */

    // this is probably response to 0738008300000064
    /* self.send(
      self.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x82, 0x00, 0x00, 0x00, 0x00],
    &[
      0x00, 0x18, 0x00, 0x03, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x18, 0x00, 0x02, 0x00, 0x00,
      0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x20
    ]).await; */

    /* self.send(
    self.device_info_destination, 0xffff, [0x07, 0x2a, 0x10, 0x09, 0x00, 0x00, 0x00, 0x00],
    &[
      0x00, 0x00, 0x00, 0x04, 0x00, 0x02, 0x00, 0x08, 0x00, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
      /* clock source: */0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x11, 0x11, 0x33,
      /* transmitting to us??? : */ 0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x11, 0x66, 0x33,
    ]).await; */
  }

  async fn send_clock_stats(&mut self) {
    let freq_offset = if let Some(f) = self.get_freq_offset_ppb() {
      f
    } else {
      return;
    };
    let required_prefix = format!("clock-stats.{}0000", hex::encode(self.self_info.mac_address.octets()));
    let mut master_clock = None;
    if let Ok(readdir) = std::fs::read_dir("/tmp") {
      for entry in readdir {
        if let Ok(entry) = entry {
          if entry.file_name().to_string_lossy().starts_with(&required_prefix) {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
              if let Some(master_id) = parse_clock_stats_master(&content) {
                master_clock = Some(master_id);
                break;
              }
            }
          }
        }
      }
    }
    if let Some(mc) = master_clock {
      let content = build_clock_status(self.self_info.mac_address.octets(), &mc, freq_offset);
      self
        .send(
          self.device_info_destination,
          0xffff,
          [0x07, 0x2a, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00],
          &content,
        )
        .await;
    }
  }

  async fn send_network_info(&mut self) {
    let mut bytes = ByteBuffer::new();
    bytes.set_endian(bytebuffer::Endian::BigEndian);
    bytes.write_bytes(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
    bytes.write_u16(self.self_info.link_speed);
    bytes.write_u16(1);
    bytes.write_bytes(&self.self_info.mac_address.octets());
    bytes.write_bytes(&self.self_info.ip_address.octets());
    bytes.write_bytes(&self.self_info.netmask.octets());
    bytes.write_bytes(&self.self_info.gateway.octets());
    bytes.write_bytes(&self.self_info.gateway.octets()); // DNS? doesn't really matter.
    bytes.write_bytes(&[
      0x00, 0x18, 0x00, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
      0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ]);

    self
      .send(
        self.device_info_destination,
        0xffff,
        [0x07, 0x2a, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00],
        bytes.as_bytes(),
      )
      .await;
  }
}

/// Clock status (conmon 0x0020) in the layout a hardware interface sends:
/// a fixed header, then one 16-byte record per PTP port, then the clock
/// identities as EUI-64. Captured from a hardware interface leading the LAN
/// (PTPv1), with the per-device fields zeroed; send_clock_stats fills them:
///   0x00        clock sync state: 1 = PLL not locked (upstream's note),
///               2 = grand leader (what the captured leader sends),
///               3 = locked follower (upstream inferno's value)
///   0x02, 0x28  port state (PTP portState: 6 = master, 9 = slave)
///   0x04        status flags
///   0x08        frequency offset, ppb (i32)
///   0x0c        own clock identity (MAC) + 2 zero bytes
///   0x14, 0x1c  leader (grandmaster) identity + 2 zero bytes
///   0x60..0x90  port records: 0x60 PTPv1 multicast (state at 0x6c),
///               0x70 PTPv2 unicast, 0x80 PTPv2 multicast (both disabled, 3)
///   0x98..0xb0  own, parent and grandmaster identities as EUI-64
/// The older header-only form left controllers without a clock role or port
/// state ("Unknown state (0x0000)"); netaudio parses this one as Follower
/// with its port records.
const CLOCK_STATUS_TEMPLATE: [u8; 188] = [
  0x00, 0x02, 0x00, 0x09, 0x00, 0x00, 0x00, 0x9f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x00
  0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x10
  0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x34, 0x00, 0x09, 0x00, 0x00, 0x02, 0x94, 0x00, 0x00, // 0x20
  0x00, 0x03, 0x0d, 0x40, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x30
  0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x60, 0x0c, 0x00, // 0x40
  0x00, 0x00, 0x00, 0x0c, 0x00, 0x98, 0x00, 0x20, 0x00, 0x03, 0x00, 0x00, 0x00, 0x68, 0x10, 0x00, // 0x50
  0x00, 0x00, 0x00, 0x01, 0x01, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x09, 0x00, 0x07, // 0x60
  0x00, 0x01, 0x00, 0x02, 0x02, 0x02, 0x02, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x03, 0x00, 0x03, // 0x70
  0x00, 0x01, 0x00, 0x03, 0x02, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x03, 0x00, 0x07, // 0x80
  0x00, 0x03, 0x00, 0x07, 0x00, 0xb8, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x90
  0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0xa0
  0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, // 0xb0
];

/// The clock status content for a follower of `leader` (the 8-byte identity
/// field statime reports: 6-byte clock id + 2 zero bytes), see
/// CLOCK_STATUS_TEMPLATE.
fn build_clock_status(own_mac: [u8; 6], leader: &[u8; 8], freq_offset_ppb: i32) -> Vec<u8> {
  const SLAVE: u16 = 9;
  // A clock status is only sent once the media clock follows the leader
  // (get_freq_offset_ppb is Some), so this device is a locked follower.
  // The template's leader value (2) paired with a follower's port state
  // made controllers show the sync status as an error.
  const LOCKED_FOLLOWER: u16 = 3;
  let mut c = CLOCK_STATUS_TEMPLATE.to_vec();
  c[0x00..0x02].copy_from_slice(&LOCKED_FOLLOWER.to_be_bytes());
  let eui64 = |id: &[u8]| [id[0], id[1], id[2], 0xff, 0xfe, id[3], id[4], id[5]];
  c[0x02..0x04].copy_from_slice(&SLAVE.to_be_bytes());
  c[0x28..0x2a].copy_from_slice(&SLAVE.to_be_bytes());
  c[0x04..0x08].copy_from_slice(&0x9fu32.to_be_bytes());
  c[0x08..0x0c].copy_from_slice(&freq_offset_ppb.to_be_bytes());
  c[0x0c..0x12].copy_from_slice(&own_mac);
  c[0x14..0x1c].copy_from_slice(leader);
  c[0x1c..0x24].copy_from_slice(leader);
  c[0x6c..0x6e].copy_from_slice(&SLAVE.to_be_bytes());
  c[0x98..0xa0].copy_from_slice(&eui64(&own_mac));
  c[0xa0..0xa8].copy_from_slice(&eui64(&leader[..6]));
  c[0xa8..0xb0].copy_from_slice(&eui64(&leader[..6]));
  c
}

/// The master clock identity (8 bytes) from the first 16 hex digits of a
/// `/tmp/clock-stats.*` file. This runs on every clock stats request from the
/// network, so a short or non-ASCII file must not panic (it used to check for
/// 12 characters, then slice 16 bytes, then assert the decoded length).
fn parse_clock_stats_master(content: &str) -> Option<[u8; 8]> {
  let digits = content.trim_ascii().get(0..16)?;
  hex::decode(digits).ok()?.try_into().ok()
}

pub async fn run_server(
  self_info: Arc<DeviceInfo>,
  mut rx: mpsc::Receiver<MulticastMessage>,
  clock: Arc<RwLock<MediaClock>>,
  mut channels_sub_rx: watch::Receiver<Option<Arc<ChannelsSubscriber>>>,
  get_peaks: PeaksCallback,
  shutdown: BroadcastReceiver<()>,
) {
  let server =
    UdpSocketWrapper::new(Some(self_info.ip_address), self_info.info_request_port, shutdown).await;
  let mut recv_buff = crate::net_utils::ReceiveBuffer::new();
  let mut mcaster = Multicaster::new(self_info.as_ref(), server, clock, get_peaks);
  mcaster.send_board_info().await;
  mcaster.send_product_info().await;
  // Announce the sample rate and encoding unprompted, as a hardware device
  // does when they change: the host changes the rate by restarting the
  // device, and a controller that is already open otherwise keeps showing
  // the old rate until it happens to probe again.
  mcaster.send_sample_rate_status().await;
  mcaster.send_encoding_status().await;
  let mut heartbeat_interval = interval(Duration::from_secs(1));
  heartbeat_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
  while mcaster.should_work() {
    select! {
      r = mcaster.server.recv(&mut recv_buff) => {
        let (_src, request_buf) = match r {
          Some(v) => v,
          None => continue
        };
        if request_buf.len() < 32 {
          error!("too short packet received: {}", hex::encode(request_buf));
          continue;
        }
        let opcode = &request_buf[24..32];
        match opcode {
          [0x07, _, 0, 0x61, 0, 0, 0, 0] => {
            mcaster.send_board_info().await;
          },
          [0x07, _, 0, 0xc1, 0, 0, 0, 0] => {
            mcaster.send_product_info().await;
          },
          [0x07, _ /* was 0x38 */, 0, 0x21, 0, 0, 0, _ /* was 0x64 */] => {
            mcaster.send_clock_stats().await;
          },
          [0x07, _, 0, 0x13, 0, 0, 0, _] => {
            mcaster.send_network_info().await;
          }
          [0x07, _, 0, 0x81, 0, 0, 0, _] => {
            // sample-rate probe (netaudio probe_sample_rate) -> status 0x80
            mcaster.send_sample_rate_status().await;
          }
          [0x07, _, 0, 0x83, 0, 0, 0, _] => {
            // encoding probe -> status 0x82
            mcaster.send_encoding_status().await;
          }
          [0x07, _, 0, 0x77, 0, 0, 0, _]=> {
            mcaster.send(
              mcaster.device_info_destination, 0xffff, [0x07, 0x2a, 0x00, 0x78, 0, 0, 0, 0],
              &[0, 0, 0, 3, 0, 0, 0, 0]
            ).await;
          }
          _ => {
            warn!("unknown request to multicast port: opcode: {}", hex::encode(opcode));
            warn!("raw udp payload: {}", hex::encode(request_buf));
          }
        };
      },
      m = rx.recv() => {
        // TODO we could also make seqnum atomic and simply share socket with anyone that wants it
        if let Some(msg) = m {
          mcaster.send(mcaster.device_info_destination, msg.start_code, msg.opcode, &msg.content).await;
        } else {
          break;
        }
      },
      _ = heartbeat_interval.tick() => {
        mcaster.send_heartbeat().await;
      },
      _ = channels_sub_rx.changed() => {
        mcaster.channels_subscriber = channels_sub_rx.borrow_and_update().clone();
      }
      // TODO receive shutdown properly, currently Ctrl-C doesn't work if there is error binding to socket
    };
  }
}

#[cfg(test)]
mod clock_stats_tests {
  use super::{build_clock_status, parse_clock_stats_master};

  #[test]
  fn clock_status_reports_a_follower_of_the_leader() {
    let own = [0xd8, 0x3a, 0xdd, 0x80, 0x89, 0xe9];
    let leader = [0x00, 0x1d, 0xc1, 0x52, 0x17, 0xc7, 0, 0];
    let c = build_clock_status(own, &leader, -13204);
    assert_eq!(c.len(), 188);
    assert_eq!(&c[0x00..0x02], &[0, 3], "sync state: locked follower, not the leader's 2");
    assert_eq!(&c[0x02..0x04], &[0, 9], "header port state: slave");
    assert_eq!(&c[0x28..0x2a], &[0, 9], "port state: slave");
    assert_eq!(&c[0x6c..0x6e], &[0, 9], "PTPv1 multicast port record: slave");
    assert_eq!(i32::from_be_bytes(c[0x08..0x0c].try_into().unwrap()), -13204);
    assert_eq!(&c[0x0c..0x14], &[0xd8, 0x3a, 0xdd, 0x80, 0x89, 0xe9, 0, 0]);
    assert_eq!(&c[0x14..0x1c], &leader);
    assert_eq!(&c[0x98..0xa0], &[0xd8, 0x3a, 0xdd, 0xff, 0xfe, 0x80, 0x89, 0xe9]);
    assert_eq!(&c[0xa8..0xb0], &[0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x52, 0x17, 0xc7]);
  }

  #[test]
  fn clock_stats_master_id() {
    assert_eq!(
      parse_clock_stats_master("  001dc1fffe112233 rest\n"),
      Some([0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x11, 0x22, 0x33])
    );
    for bad in ["", "001dc1fffe11", "001dc1fffe11223", "zz1dc1fffe112233", "001dc1fffe1122ä3", "ääääääää"]
    {
      assert_eq!(parse_clock_stats_master(bad), None, "{bad:?}");
    }
  }
}

/// Update modes of the capability status replies, as the LAN's hardware
/// interface reports them (sample rate 0, encoding 1). The earlier fixed 2
/// came from an upstream capture comment, not from a device.
const SAMPLE_RATE_UPDATE_MODE: u16 = 0;
const ENCODING_UPDATE_MODE: u16 = 1;

/// Content of an audio capability status reply: u16 0x0018, u16 count,
/// u32 current, u32 requested, u16 update mode, u16 0, then count x u32
/// supported values (field names from netaudio's parser). Requested is the
/// current value: no change is ever pending. It used to be 0, which a
/// controller reads as a pending change to an invalid value.
fn capability_status_content(current: u32, update_mode: u16, supported: &[u32]) -> Vec<u8> {
  let mut content = Vec::with_capacity(16 + 4 * supported.len());
  content.extend_from_slice(&0x0018u16.to_be_bytes());
  content.extend_from_slice(&(supported.len() as u16).to_be_bytes());
  content.extend_from_slice(&current.to_be_bytes());
  content.extend_from_slice(&current.to_be_bytes());
  content.extend_from_slice(&update_mode.to_be_bytes());
  content.extend_from_slice(&0u16.to_be_bytes());
  for v in supported {
    content.extend_from_slice(&v.to_be_bytes());
  }
  content
}

#[cfg(test)]
mod capability_tests {
  use super::{capability_status_content, BOARD_PRIMARY_CAPABILITIES};

  #[test]
  fn board_info_advertises_rate_and_encoding_configuration() {
    // bytes 0x14..0x18 of the board info: manufacturer name (0x10 in
    // byte 0x16), sample rate (0x08) and encoding (0x10) in byte 0x17
    assert_eq!(BOARD_PRIMARY_CAPABILITIES.to_be_bytes(), [0x00, 0x00, 0x10, 0x18]);
  }

  #[test]
  fn sample_rate_status_layout() {
    let c = capability_status_content(48000, 0, &[48000]);
    assert_eq!(
      c,
      [
        0x00, 0x18, 0x00, 0x01, // marker, one supported value
        0x00, 0x00, 0xbb, 0x80, // current 48000
        0x00, 0x00, 0xbb, 0x80, // requested 48000: nothing pending
        0x00, 0x00, 0x00, 0x00, // update mode 0
        0x00, 0x00, 0xbb, 0x80, // supported 48000
      ]
    );
  }

  #[test]
  fn encoding_status_lists_every_supported_value() {
    let c = capability_status_content(24, 1, &[16, 24, 32]);
    assert_eq!(c.len(), 16 + 12);
    assert_eq!(&c[2..4], &[0, 3]);
    assert_eq!(&c[4..8], &24u32.to_be_bytes());
    assert_eq!(&c[8..12], &24u32.to_be_bytes());
    assert_eq!(&c[12..14], &[0, 1]);
    assert_eq!(&c[16..], &[0, 0, 0, 16, 0, 0, 0, 24, 0, 0, 0, 32]);
  }
}

#[cfg(test)]
mod capability_word_tests {
  use super::{primary_capabilities, CapabilityFeatures};

  #[test]
  fn default_features_match_legacy_word() {
    let f = CapabilityFeatures { manufacturer_name: true, sample_rate_config: true, encoding_config: true, ..Default::default() };
    assert_eq!(primary_capabilities(&f), 0x0000_1000 | 0x0000_0008 | 0x0000_0010);
  }

  #[test]
  fn serial_leds_metering_do_not_set_primary_bits() {
    let f = CapabilityFeatures { serial_ports: true, leds: true, metering: true, ..Default::default() };
    assert_eq!(primary_capabilities(&f), 0);
  }

  #[test]
  fn no_features_yields_zero() {
    assert_eq!(primary_capabilities(&CapabilityFeatures::default()), 0);
  }

  #[test]
  fn identify_bit() {
    let f = CapabilityFeatures { identify: true, ..Default::default() };
    assert_eq!(primary_capabilities(&f) & 0x0000_0001, 0x0000_0001);
  }

  #[test]
  fn aes67_and_lock_bits() {
    let f = CapabilityFeatures { aes67: true, locking: true, ..Default::default() };
    let w = primary_capabilities(&f);
    assert_eq!(w & 0x0400_0000, 0x0400_0000);
    assert_eq!(w & 0x0800_0000, 0x0800_0000);
  }
}

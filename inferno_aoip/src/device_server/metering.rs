//! Metering configuration and re-framing helpers (handoff W5).
//!
//! Conmon's `METERING_STATUS` carries per-channel level (printed `%4.1f` in
//! firmware) plus independent CLIP/MUTE/SOM bits, with configurable
//! `update_rate`, `peak_holdoff` and `peak_decay`. Inferno's info-plane peaks
//! must be re-framed into this shape; this module holds the pure, testable
//! parts.

/// Per-channel metering flags (firmware: CLIP, MUTE, SOM).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct MeterFlags {
  pub clip: bool,
  pub mute: bool,
  pub som: bool,
}

impl MeterFlags {
  pub fn to_byte(self) -> u8 {
    (self.clip as u8) | ((self.mute as u8) << 1) | ((self.som as u8) << 2)
  }
  pub fn from_byte(b: u8) -> Self {
    Self { clip: b & 1 != 0, mute: b & 2 != 0, som: b & 4 != 0 }
  }
}

/// Metering configuration written by DC's meter settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeteringConfig {
  pub update_rate: u32,    // Hz
  pub peak_holdoff: f32,   // seconds
  pub peak_decay: f32,     // dB/s
}

impl Default for MeteringConfig {
  fn default() -> Self {
    Self { update_rate: 10, peak_holdoff: 1.0, peak_decay: 20.0 }
  }
}

impl MeteringConfig {
  /// Validate ranges; returns an error description if invalid.
  pub fn validate(&self) -> Result<(), &'static str> {
    if self.update_rate == 0 {
      return Err("update_rate must be > 0");
    }
    if !(0.0..=10.0).contains(&self.peak_holdoff) {
      return Err("peak_holdoff out of range");
    }
    if self.peak_decay <= 0.0 {
      return Err("peak_decay must be > 0");
    }
    Ok(())
  }
}

/// Convert a linear peak (0.0..=1.0) to the firmware's `%4.1f` dB level.
pub fn peak_to_db_level(peak_linear: f32) -> f32 {
  if !peak_linear.is_finite() || peak_linear <= 0.0 {
    return -99.9;
  }
  let peak = peak_linear.min(1.0);
  (peak.log10() * 20.0).max(-99.9).min(0.0)
}

/// Pack per-channel levels + flags into a METERING_STATUS-style payload:
/// for each channel: f32 level (big-endian) + u8 flags.
pub fn metering_status_payload(levels_db: &[f32], flags: &[MeterFlags]) -> Vec<u8> {
  let n = levels_db.len().min(flags.len());
  let mut out = Vec::with_capacity(n * 5);
  for i in 0..n {
    out.extend_from_slice(&levels_db[i].to_be_bytes());
    out.push(flags[i].to_byte());
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn flags_ignore_high_bits() {
    assert_eq!(MeterFlags::from_byte(0xFF).to_byte(), 0x07);
  }

  #[test]
  fn flags_roundtrip() {
    for b in 0u8..8 {
      assert_eq!(MeterFlags::from_byte(b).to_byte(), b);
    }
  }

  #[test]
  fn config_default_valid() {
    assert!(MeteringConfig::default().validate().is_ok());
  }

  #[test]
  fn config_invalid_update_rate() {
    let c = MeteringConfig { update_rate: 0, ..Default::default() };
    assert!(c.validate().is_err());
  }

  #[test]
  fn config_invalid_peak_decay() {
    let c = MeteringConfig { peak_decay: 0.0, ..Default::default() };
    assert!(c.validate().is_err());
  }

  #[test]
  fn peak_to_db_silence() {
    assert_eq!(peak_to_db_level(0.0), -99.9);
  }

  #[test]
  fn peak_to_db_negative_or_nan_is_silence() {
    assert_eq!(peak_to_db_level(-1.0), -99.9);
    assert_eq!(peak_to_db_level(f32::NAN), -99.9);
    assert_eq!(peak_to_db_level(f32::INFINITY), -99.9);
  }

  #[test]
  fn peak_to_db_clamps_above_unity() {
    // >1.0 should not produce positive dB
    assert!(peak_to_db_level(2.0) <= 0.0);
  }

  #[test]
  fn peak_to_db_full_scale() {
    assert!((peak_to_db_level(1.0) - 0.0).abs() < 1e-3);
  }

  #[test]
  fn payload_length() {
    let levels = [0.0f32, -6.0];
    let flags = [MeterFlags::default(), MeterFlags { clip: true, ..Default::default() }];
    let p = metering_status_payload(&levels, &flags);
    assert_eq!(p.len(), 2 * 5);
    assert_eq!(p[4], 0);
    assert_eq!(p[9], 1);
  }

  #[test]
  fn payload_truncates_to_min_len() {
    let levels = [0.0f32, -6.0, -12.0];
    let flags = [MeterFlags::default()];
    let p = metering_status_payload(&levels, &flags);
    assert_eq!(p.len(), 1 * 5);
  }
}

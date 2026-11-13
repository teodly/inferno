use std::sync::Arc;

use crate::{
  common::Sample,
  ring_buffer::{ProxyToSamplesBuffer, RingBufferShared},
};

pub fn peaks_of_buffers<P: ProxyToSamplesBuffer>(
  buffers: &Vec<Arc<RingBufferShared<Sample, P>>>,
) -> Vec<u8> {
  // Precompute the reciprocal once instead of dividing per channel.
  let inv_max = 1.0f32 / (Sample::MAX as f32);
  buffers
    .iter()
    .map(|rb| {
      let peak = rb.peak_sample();
      let peak_lin = (peak as f32) * inv_max;
      let peak_log = peak_lin.log10() * 40.0;
      (-peak_log).round().clamp(0.0, 255.0) as u8
    })
    .collect()
}

use crate::common::*;
use rand::Rng;

pub struct SamplesReader<'a> {
  pub bytes: &'a [u8],
  pub read_pos: usize,
  pub stride: usize,
  pub remaining_samples: usize,
}

impl<'a> SamplesReader<'a> {
  #[inline(always)]
  fn get_next_bytes(&mut self, count: usize) -> Option<&'a [u8]> {
    if self.read_pos + count > self.bytes.len() {
      return None;
    }
    let r = &self.bytes[self.read_pos..self.read_pos + count];
    self.read_pos += self.stride;
    self.remaining_samples -= 1;
    return Some(r);
  }
  #[inline(always)]
  fn size_hint(&self) -> (usize, Option<usize>) {
    let size = self.remaining_samples;
    (size, Some(size))
  }
}

macro_rules! samples_rw {
  ($bytes: literal, $reader_iterator: ident, $to_sample: expr, $writer_function: ident, $dither_type: ident) => {
    pub struct $reader_iterator<'a>(pub SamplesReader<'a>);
    impl<'a> Iterator for $reader_iterator<'a> {
      type Item = Sample;
      #[inline(always)]
      fn next(&mut self) -> Option<Sample> {
        self
          .0
          .get_next_bytes($bytes)
          .map($to_sample)
          .map(|s| s as Sample)
      }
      #[inline(always)]
      fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
      }
    }
    impl<'a> ExactSizeIterator for $reader_iterator<'a> {}

    #[inline(always)]
    pub fn $writer_function<'a, I, R>(src: I, dst: &mut [u8], start_pos: usize, stride: usize, mut dither_rng: Option<&mut R>)
    where I: IntoIterator<Item=&'a Sample>, R: Rng {
      let mut pos = start_pos;
      let mut srci = src.into_iter();
      let half_step: Sample = 1 << ($dither_type::BITS-1);
      while pos + $bytes <= dst.len() {
        if let Some(&sample) = srci.next() {
          let out_sample = match &mut dither_rng {
            Some(rng) if $bytes < 4 => sample.saturating_add((rng.gen::<$dither_type>() as Sample) - (rng.gen::<$dither_type>() as Sample) + half_step),
            _ => sample
          };
          dst[pos..pos+$bytes].copy_from_slice(&out_sample.to_be_bytes()[0..$bytes]);
          pos += stride;
        } else {
          break;
        }
      }
    }
  }
}

samples_rw!(2, S16ReaderIterator, |b|
  ((b[0] as USample) << 24) |
  ((b[1] as USample) << 16),
  write_s16_samples,
  u16
);
samples_rw!(3, S24ReaderIterator, |b|
  ((b[0] as USample) << 24) | 
  ((b[1] as USample) << 16) | 
  ((b[2] as USample) << 8),
  write_s24_samples,
  u8
);
samples_rw!(4, S32ReaderIterator, |b|
  ((b[0] as USample) << 24) |
  ((b[1] as USample) << 16) |
  ((b[2] as USample) << 8) |
  (b[3] as USample),
  write_s32_samples,
  u8 // won't be used anyway
);

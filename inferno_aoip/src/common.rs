use std::fmt::Display;

pub use log::{debug, error, info, trace, warn};
pub type Sample = i32;
pub type USample = u32;

/// Audio clock (number of samples since arbitrary epoch)
pub type Clock = u64;

/// Signed version of the clock. For clock deltas.
pub type ClockDiff = i64;

/// Subtract clocks and return the result as a signed number.
/// Hint: wrapped `a > b` is equivalent to `wrapped_diff(a, b) > 0`
pub fn wrapped_diff(a: Clock, b: Clock) -> ClockDiff {
  (a as ClockDiff).wrapping_sub(b as ClockDiff)
}

pub trait LogAndForget {
  fn log_and_forget(&self);
}

impl<T, E: std::fmt::Debug> LogAndForget for Result<T, E> {
  fn log_and_forget(&self) {
    if let Err(e) = self {
      warn!("Encountered error {e:?} at {:?}", std::backtrace::Backtrace::capture());
    }
  }
}

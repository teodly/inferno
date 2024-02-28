use std::fmt::Display;

pub use log::{debug, error, info, trace, warn};
pub type Sample = i32;
pub type USample = u32;

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

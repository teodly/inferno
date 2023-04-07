use crate::common::*;
use atomic::{Atomic, Ordering};

struct FragmentDescriptor {
  start: Atomic<usize>,
  end: Atomic<usize>
}
impl FragmentDescriptor {
  fn clear(&self) {
    self.end.store(0, Ordering::Release);
  }
  fn is_some(&self) -> bool {
    self.start.load(Ordering::Acquire) < self.end.load(Ordering::Acquire)
  }
  fn is_none(&self) -> bool {
    !self.is_some()
  }
}

struct ChannelRxBuffer {
  buffer: Vec<Atomic<Sample>>,
  ready_fragments: Vec<FragmentDescriptor>
}

impl ChannelRxBuffer {
  
}

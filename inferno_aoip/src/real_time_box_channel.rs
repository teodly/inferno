//! Used for decoupling memory allocation and deallocation from realtime thread.
//! Sender is non-real-time, receiver is real-time.

use std::{mem, sync::Arc};

use atomic::Ordering;
use atomicbox::AtomicOptionBox;

struct Shared<T> {
  next: AtomicOptionBox<T>,
  prev: AtomicOptionBox<T>,
}

pub fn channel<T>(initial: Box<T>) -> (RealTimeBoxSender<T>, RealTimeBoxReceiver<T>) {
  let shared = Arc::new(Shared {
    next: AtomicOptionBox::new(None),
    prev: AtomicOptionBox::new(None),
  });
  let sender = RealTimeBoxSender(shared.clone());
  let receiver = RealTimeBoxReceiver{ shared, curr: initial };
  (sender, receiver)
}

pub struct RealTimeBoxSender<T>(Arc<Shared<T>>);

impl<T> RealTimeBoxSender<T> {
  pub fn send(&self, value: Box<T>) {
    self.collect_garbage();
    let unused_next = self.0.next.swap(Some(value), Ordering::AcqRel);
    drop(unused_next);
  }
  pub fn collect_garbage(&self) {
    let prev = self.0.prev.take(Ordering::AcqRel);
    drop(prev);
  }
}

pub struct RealTimeBoxReceiver<T> {
  shared: Arc<Shared<T>>,
  curr: Box<T>,
}

impl<T> RealTimeBoxReceiver<T> {
  pub fn update(&mut self) -> bool {
    if let Some(prev) = self.shared.prev.take(Ordering::AcqRel) {
      // prev hasn't been received by non-real-time thread yet, so we can't proceed
      // reinstall the prev back
      let swapped_prev = self.shared.prev.swap(Some(prev), Ordering::AcqRel);
      debug_assert!(swapped_prev.is_none()); // only this thread is allowed to write Some(...) to prev
      false
    } else {
      // we have space in prev to put current Box into
      if let Some(next) = self.shared.next.take(Ordering::AcqRel) {
        let prev = mem::replace(&mut self.curr, next);
        let swapped_prev = self.shared.prev.swap(Some(prev), Ordering::AcqRel);
        debug_assert!(swapped_prev.is_none());
        true
      } else {
        false
      }
    }
  }
  pub fn get(&self) -> &T {
    self.curr.as_ref()
  }
  pub fn get_mut(&mut self) -> &mut T {
    self.curr.as_mut()
  }
}

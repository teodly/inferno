use crate::common::*;
use atomic::{Atomic, Ordering};
use bool_vec::{boolvec, BoolVec};
use bytemuck::NoUninit;
use itertools::Itertools;
use std::{
  marker::PhantomData,
  slice,
  sync::{atomic::AtomicUsize, Arc, RwLock},
};

pub fn wrapsub(a: usize, b: usize) -> isize {
  (a as isize).wrapping_sub(b as isize)
}

pub trait ProxyToBuffer<T> {
  fn len(&self) -> usize;
  /// If buffer is available, executes `cb` with buffer's slice as an argument and returns Some with its result
  /// If buffer is unavailable, returns None
  fn map<R>(&self, cb: impl FnOnce(&[T]) -> R) -> Option<R>;
  fn unconditional_read(&self) -> bool;
}

pub trait ProxyToSamplesBuffer: ProxyToBuffer<Atomic<Sample>> {}

/// A buffer which owns its data, stored as a `Vec`
pub struct OwnedBuffer<T>(pub Vec<T>);

impl<T: Default> OwnedBuffer<T> {
  fn new(length: usize) -> Self {
    Self((0..length).map(|_| T::default()).collect_vec())
  }
}

impl<T> ProxyToBuffer<T> for OwnedBuffer<T> {
  #[inline(always)]
  fn len(&self) -> usize {
    self.0.len()
  }
  #[inline(always)]
  fn map<R>(&self, cb: impl FnOnce(&[T]) -> R) -> Option<R> {
    Some(cb(self.0.as_slice()))
  }
  #[inline(always)]
  fn unconditional_read(&self) -> bool {
    false
  }
}

impl ProxyToSamplesBuffer for OwnedBuffer<Atomic<Sample>> {}

/// Buffer which can be invalidated at any time by an external force.
/// Intended for buffers managed by libraries not written in Rust.
pub struct ExternalBuffer<T> {
  ptr: *const T,
  length: usize,
  // we're using separate flag because ExternalBuffer may be one of the views of the same interleaved buffer with different offsets
  valid: Arc<RwLock<bool>>,
}

// safety: is guaranteed by lockable valid flag
unsafe impl<T> Send for ExternalBuffer<T> {}
unsafe impl<T> Sync for ExternalBuffer<T> {}

impl<T> ExternalBuffer<T> {
  /// `ptr` is pointer to start of the buffer
  ///
  /// `length` is buffer length in items (not in bytes)
  ///
  /// `valid` is shared and locked reference to flag which should be set to `false` when we are notified that buffer is no longer valid.
  /// (it can't be atomic_bool by design - it must be locked all time the buffer is in use)
  ///
  /// Safety: user must ensure that ptr & length correspond to a valid memory region containing a slice `[T; length]`
  unsafe fn new(ptr: *const T, length: usize, valid: Arc<RwLock<bool>>) -> Self {
    Self { ptr, length, valid }
  }
}

impl<T> ProxyToBuffer<T> for ExternalBuffer<T> {
  #[inline(always)]
  fn len(&self) -> usize {
    self.length
  }
  #[inline(always)]
  fn map<R>(&self, cb: impl FnOnce(&[T]) -> R) -> Option<R> {
    if let Ok(guard) = self.valid.try_read() {
      let valid = *guard;
      if valid {
        Some(cb(unsafe { slice::from_raw_parts(self.ptr, self.length) }))
      } else {
        None
      }
    } else {
      None
    }
  }
  #[inline(always)]
  fn unconditional_read(&self) -> bool {
    true
  }
}

impl ProxyToSamplesBuffer for ExternalBuffer<Atomic<Sample>> {}

#[derive(Clone)]
pub struct PositionReportDestination {
  tab: Arc<Vec<AtomicUsize>>,
  offset: usize,
}

impl PositionReportDestination {
  pub fn new(tab: Arc<Vec<AtomicUsize>>, offset: usize) -> Self {
    Self { tab, offset }
  }
}

pub struct RingBufferShared<T, P: ProxyToBuffer<Atomic<T>>> {
  _t: PhantomData<T>,
  buffer: P,
  stride: usize,
  items_size: usize,
  readable_pos: AtomicUsize,
  writing_pos: AtomicUsize,
  holes_count: AtomicUsize,

  readable_pos_dest: Option<PositionReportDestination>,
}

impl<T, P: ProxyToBuffer<Atomic<T>>> RingBufferShared<T, P> {
  fn new(
    storage: P,
    stride: usize,
    start_time: usize,
    readable_pos_dest: Option<PositionReportDestination>,
  ) -> Arc<Self> {
    let items_size = (storage.len() + stride - 1) / stride;
    assert!(items_size.is_power_of_two());
    Arc::new(RingBufferShared {
      _t: Default::default(),
      buffer: storage,
      stride,
      items_size,
      readable_pos: start_time.into(),
      writing_pos: start_time.into(),
      holes_count: 0.into(),
      readable_pos_dest,
    })
  }
  #[inline(always)]
  fn item_to_buffer_index(&self, i: usize) -> usize {
    // a % b == a & (b-1) if b is power of 2
    (i & (self.items_size - 1)) * self.stride
  }
  #[inline(always)]
  fn commit_readable_pos(&self) {
    if let Some(dest) = &self.readable_pos_dest {
      dest.tab[dest.offset].store(self.readable_pos.load(Ordering::Relaxed), Ordering::Release);
    }
  }

  pub fn reset(&self, start_time: usize) {
    self.writing_pos.store(start_time, Ordering::Relaxed);
    self.readable_pos.store(start_time, Ordering::Relaxed);
    atomic::fence(Ordering::SeqCst);
  }
}

#[inline(always)]
fn for_in_ring(length: usize, start: usize, end: usize, mut cb: impl FnMut(usize)) {
  if start == end {
    return;
  }
  let length_mask = length - 1;
  let w_start = start & length_mask;
  let w_end = end & length_mask;
  if w_start < w_end {
    for i in w_start..w_end {
      cb(i);
    }
  } else {
    for i in w_start..length {
      cb(i);
    }
    for i in 0..w_end {
      cb(i);
    }
  }
}

impl<P: ProxyToBuffer<Atomic<Sample>>> RingBufferShared<Sample, P> {
  pub fn peak_sample(&self) -> Sample {
    self
      .buffer
      .map(|buffer| {
        buffer
          .iter()
          .step_by(self.stride)
          .map(|s| s.load(Ordering::Relaxed).saturating_abs())
          .max()
          .unwrap_or(0)
      })
      .unwrap_or(0)
  }
}

pub struct RBInput<T, P: ProxyToBuffer<Atomic<T>>> {
  rb: Arc<RingBufferShared<T, P>>,
  item_ready: BoolVec,
  hole_fix_wait: usize,
}

impl<T: Default + NoUninit, P: ProxyToBuffer<Atomic<T>>> RBInput<T, P> {
  pub fn new(shared: Arc<RingBufferShared<T, P>>, hole_fix_wait: usize) -> Self {
    let items_size = shared.items_size;
    Self { rb: shared, item_ready: boolvec![false; items_size], hole_fix_wait }
  }

  #[inline(always)]
  pub fn ring_buffer_size(&self) -> usize {
    self.rb.items_size
  }
  #[inline(always)]
  pub fn has_same_ring_buffer(&self, other: &RBInput<T, P>) -> bool {
    Arc::ptr_eq(&self.rb, &other.rb)
  }
  #[inline(always)]
  pub fn shared(&self) -> &Arc<RingBufferShared<T, P>> {
    &self.rb
  }

  /// returns the position that can be read using accompanying RBOutput
  pub fn write_from_at(
    &mut self,
    start_timestamp: usize,
    mut input: impl ExactSizeIterator<Item = T>,
  ) -> usize {
    let input_len = input.len();
    assert!(input_len < self.rb.items_size);

    // Do we have a new hole?
    let mut hole = wrapsub(start_timestamp, self.rb.writing_pos.load(Ordering::Relaxed)) > 0;
    if hole {
      // Mark it appropriately.
      for_in_ring(
        self.rb.items_size,
        self.rb.writing_pos.load(Ordering::Relaxed),
        start_timestamp,
        |i| {
          self.item_ready.set(i, false);
        },
      );
    }

    // Did we have a hole before current invocation?
    hole |= self.rb.readable_pos.load(Ordering::Relaxed) != self.rb.writing_pos.load(Ordering::Relaxed);

    let end_ts = start_timestamp.wrapping_add(input_len);

    // Update writing_pos to let RBOutput know that that we're going to write some items
    // but make sure we don't go back in time when fixing a hole
    if wrapsub(end_ts, self.rb.writing_pos.load(Ordering::Relaxed)) > 0 {
      self.rb.writing_pos.store(end_ts, Ordering::SeqCst);
    }

    // Write the items:
    self.rb.buffer.map(|buffer| {
      for_in_ring(self.rb.items_size, start_timestamp, end_ts, |i| {
        //debug!("writing to RB index {i}");
        buffer[i * self.rb.stride].store(input.next().unwrap(), Ordering::Relaxed);
        self.item_ready.set(i, true);
      });
      atomic::fence(Ordering::Release); // TODO: really needed? won't readable_pos.store(Ordering::Release) suffice?
    });

    // Inform the RBOutput that new data is readable
    // but do it only if there are no holes
    if !hole {
      self.rb.readable_pos.store(self.rb.writing_pos.load(Ordering::Relaxed), Ordering::Release);
    }

    // If there was a hole, close (write Default to) too old items
    if hole {
      self.close_items_until_internal(
        self.rb.writing_pos.load(Ordering::Relaxed).wrapping_sub(self.hole_fix_wait),
        self.rb.writing_pos.load(Ordering::Relaxed),
      );
    }

    self.rb.commit_readable_pos();
    self.rb.readable_pos.load(Ordering::Relaxed)
  }

  fn close_items_until_internal(&mut self, close_until_pos: usize, check_until_pos: usize) {
    // Put default values in any holes in readable_pos..close_until_pos range:
    if wrapsub(close_until_pos, self.rb.readable_pos.load(Ordering::Relaxed)) > 0 {
      let mut hole = false;
      self.rb.buffer.map(|buffer| {
        for_in_ring(
          self.rb.items_size,
          self.rb.readable_pos.load(Ordering::Relaxed),
          close_until_pos,
          |i| {
            if !self.item_ready.get(i).unwrap() {
              hole = true;
              buffer[i * self.rb.stride].store(T::default(), Ordering::Relaxed);
              self.item_ready.set(i, true);
            }
          },
        );
      });
      if hole {
        self.rb.holes_count.fetch_add(1, Ordering::Release);
      }
      atomic::fence(Ordering::Release); // TODO: really needed? won't readable_pos.store(Ordering::Release) suffice?
      self.rb.readable_pos.store(close_until_pos, Ordering::Release);
    }

    // Check for fixed holes and update readable_pos accordingly:
    if wrapsub(check_until_pos, self.rb.readable_pos.load(Ordering::Relaxed)) > 0 {
      let mut ready = true;
      let mut new_readable_pos = self.rb.readable_pos.load(Ordering::Relaxed);
      for_in_ring(
        self.rb.items_size,
        self.rb.readable_pos.load(Ordering::Relaxed),
        check_until_pos,
        |i| {
          if !ready {
            return;
          }
          if self.item_ready.get(i).unwrap() {
            new_readable_pos += 1;
          } else {
            ready = false;
          }
        },
      );
      self.rb.readable_pos.store(new_readable_pos, Ordering::Release);
    }
  }

  pub fn close_items_until(&mut self, mut until_pos: usize) {
    let writing_pos = self.rb.writing_pos.load(Ordering::Relaxed);
    //debug!("writing_pos {writing_pos}, until_pos {until_pos}");
    let need_until_pos = until_pos;
    if wrapsub(until_pos, writing_pos) > 0 {
      until_pos = writing_pos;
    }

    self.close_items_until_internal(until_pos, until_pos);

    // If closing not-yet-touched items was requested, do it:
    if need_until_pos != until_pos {
      self.rb.writing_pos.store(need_until_pos, Ordering::SeqCst);
      //debug!("storing defaults {until_pos}..{need_until_pos}");
      self.rb.buffer.map(|buffer| {
        for_in_ring(self.rb.items_size, until_pos, need_until_pos, |i| {
          buffer[i * self.rb.stride].store(T::default(), Ordering::Relaxed);
          self.item_ready.set(i, true);
        })
      });
      self.rb.readable_pos.store(need_until_pos, Ordering::Release);
    }

    self.rb.commit_readable_pos();
  }
}

/// Result of the `RBOutput::read_at` function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadResult {
  /// start index of useful data in output slice
  pub useful_start_index: usize,
  /// end index of useful data in output slice
  pub useful_end_index: usize,
}

pub struct RBOutput<T, P: ProxyToBuffer<Atomic<T>>> {
  rb: Arc<RingBufferShared<T, P>>,
}

impl<T, P: ProxyToBuffer<Atomic<T>>> Clone for RBOutput<T, P> {
  // https://stackoverflow.com/questions/72150623/deriveclone-seems-to-wrongfully-enforce-generic-to-be-clone
  fn clone(&self) -> Self {
    Self { rb: self.rb.clone() }
  }
}

impl<T: NoUninit, P: ProxyToBuffer<Atomic<T>>> RBOutput<T, P> {
  #[inline(always)]
  pub fn shared(&self) -> &Arc<RingBufferShared<T, P>> {
    &self.rb
  }
  pub fn readable_until(&self) -> usize {
    self.rb.readable_pos.load(Ordering::Acquire)
  }
  /// Because single ring buffer with its read position pointer can be shared between multiple `RBOutput`s,
  /// this method, unlike `RBInput::write_from_at`, does not advance the read position.
  /// Use the method `read_done` when reads of all `RBOutput`s sharing the same ring buffer are done.
  pub fn read_at(&self, start_timestamp: usize, output: &mut [T]) -> ReadResult {
    // in normal case: start_ts < readable_pos <= writing_pos
    let output_len = if self.rb.buffer.unconditional_read() {
      output.len()
    } else {
      let readable = wrapsub(self.rb.readable_pos.load(Ordering::Acquire), start_timestamp);
      if readable < 0 || readable > self.rb.items_size.try_into().unwrap() {
        debug!("readable {readable}, items_size {}", self.rb.items_size);
        // we're trying to read not yet written data, or we're lagging behind so much that data at that timestamp has already been overwritten
        return ReadResult { useful_start_index: 0, useful_end_index: 0 };
      }
      let readable = readable as usize;
      let original_output_len = output.len();
      original_output_len.min(readable)
    };

    //let wrapped_start_ts = start_timestamp % self.rb.items_size;
    //let wrapped_end_ts = start_timestamp.wrapping_add(output_len) % self.rb.items_size;
    let mut out_index = 0;

    if self
      .rb
      .buffer
      .map(|buffer| {
        atomic::fence(Ordering::Acquire);
        for_in_ring(
          self.rb.items_size,
          start_timestamp,
          start_timestamp.wrapping_add(output_len),
          |rb_index| {
            //debug!("reading from RB index {rb_index}");
            output[out_index] = buffer[rb_index * self.rb.stride].load(Ordering::Relaxed);
            out_index += 1;
          },
        );
      })
      .is_none()
    {
      return ReadResult { useful_start_index: 0, useful_end_index: 0 };
    }

    if !self.rb.buffer.unconditional_read() {
      let writing_pos = self.rb.writing_pos.load(Ordering::SeqCst);
      let diff = wrapsub(writing_pos, start_timestamp);
      if diff > self.rb.items_size.try_into().unwrap() {
        // data has been overwritten in the meantime, so some data at the beginning of the buffer may be wrong
        let overwritten = diff as usize - self.rb.items_size;
        return ReadResult {
          useful_start_index: overwritten.min(output_len),
          useful_end_index: output_len,
        };
      }
    }

    ReadResult { useful_start_index: 0, useful_end_index: output_len }
  }

  pub fn read_done(&self, _until: usize) {
    //self.rb.read_pos.store(until, Ordering::Release);
  }
  pub fn holes_count(&self) -> usize {
    self.rb.holes_count.load(Ordering::Acquire)
  }
}

pub fn new_owned<T: Default>(
  length: usize,
  start_time: usize,
  hole_fix_wait: usize,
) -> (RBInput<T, OwnedBuffer<Atomic<T>>>, RBOutput<T, OwnedBuffer<Atomic<T>>>) {
  let shared = RingBufferShared::new(OwnedBuffer::<Atomic<T>>::new(length), 1, start_time, None);
  (
    RBInput { rb: shared.clone(), item_ready: boolvec![false; shared.items_size], hole_fix_wait },
    RBOutput { rb: shared },
  )
}

pub struct ExternalRBInput<T> {
  rb: Arc<RingBufferShared<T, ExternalBuffer<Atomic<T>>>>,
  margin: usize,
}

impl<T> ExternalRBInput<T> {
  fn advance(&self, new_position: usize) {
    self.rb.writing_pos.store(new_position, Ordering::Release);
    self.rb.readable_pos.store(new_position.wrapping_sub(self.margin), Ordering::Release);
    self.rb.commit_readable_pos();
  }
  fn position(&self, clock: usize) -> usize {
    clock
  }
}

pub struct ExternalRBOutput<T> {
  rb: Arc<RingBufferShared<T, ExternalBuffer<Atomic<T>>>>,
}

impl<T> ExternalRBOutput<T> {
  fn position(&self, _clock: usize) -> usize {
    self.rb.readable_pos.load(Ordering::Acquire)
    // TODO when no data is received and so readable_pos can't be advanced by normal means, fill with silence
  }
}

//#[derive(Clone)]
pub struct ExternalBufferParameters<T> {
  ptr: *const Atomic<T>,
  length: usize,
  stride: usize,
  valid: Arc<RwLock<bool>>,
  readable_pos_dest: Option<PositionReportDestination>,
}

// safety: is guaranteed by lockable valid flag
unsafe impl<T> Send for ExternalBufferParameters<T> {}
unsafe impl<T> Sync for ExternalBufferParameters<T> {}

impl<T> ExternalBufferParameters<T> {
  pub unsafe fn new(
    ptr: *const Atomic<T>,
    length: usize,
    stride: usize,
    valid: Arc<RwLock<bool>>,
    readable_pos_dest: Option<PositionReportDestination>,
  ) -> Self {
    Self { ptr, length, stride, valid, readable_pos_dest }
  }
}

// safety: ExternalBufferParameters::new is unsafe so user acknowledges the dangers when creating the `par` struct
pub fn shared_from_external<T: Default>(
  par: &ExternalBufferParameters<T>,
  start_time: usize,
) -> Arc<RingBufferShared<T, ExternalBuffer<Atomic<T>>>> {
  let external = unsafe { ExternalBuffer::<Atomic<T>>::new(par.ptr, par.length, par.valid.clone()) };
  RingBufferShared::new(external, par.stride, start_time, par.readable_pos_dest.clone())
}

pub fn wrap_external_source<T: Default>(
  par: &ExternalBufferParameters<T>,
  start_time: usize,
) -> RBOutput<T, ExternalBuffer<Atomic<T>>> {
  RBOutput { rb: shared_from_external(par, start_time) }
}

pub fn wrap_external_sink<T: Default>(
  par: &ExternalBufferParameters<T>,
  start_time: usize,
  hole_fix_wait: usize,
) -> RBInput<T, ExternalBuffer<Atomic<T>>> {
  let shared = shared_from_external(par, start_time);
  let items_size = shared.items_size;
  RBInput { rb: shared, item_ready: boolvec![false; items_size], hole_fix_wait }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{Arc, Barrier};
  use std::thread;

  #[test]
  fn test_sequential_single_write_read() {
    let (mut input, output) = new_owned(16, 0, 4);

    let writer = thread::spawn(move || {
      for i in 0..16 {
        input.write_from_at(i, std::iter::once(i as i32));
      }
    });

    let reader = thread::spawn(move || {
      let mut read_values = vec![0; 16];
      for i in 0..16 {
        while output.read_at(i, &mut read_values[i..i + 1])
          != (ReadResult { useful_start_index: 0, useful_end_index: 1 })
        {
          thread::yield_now();
        }
      }
      assert_eq!(read_values, (0..16).collect::<Vec<_>>());
    });

    writer.join().unwrap();
    reader.join().unwrap();
  }

  fn non_sequential_write_single_read(wait: usize, expected: Vec<i32>) {
    let (mut input, output) = new_owned(16, 0, wait);

    let barrier = Arc::new(Barrier::new(2));
    let barrier_writer = barrier.clone();
    let barrier_reader = barrier.clone();

    let writer = thread::spawn(move || {
      barrier_writer.wait();
      input.write_from_at(4, (4..8).map(|x| x as i32));
      input.write_from_at(0, (0..4).map(|x| x as i32));
    });

    let reader = thread::spawn(move || {
      barrier_reader.wait();
      let mut read_values = vec![0; 8];
      for i in 0..8 {
        while output.read_at(i, &mut read_values[i..i + 1])
          != (ReadResult { useful_start_index: 0, useful_end_index: 1 })
        {
          thread::yield_now();
        }
      }
      assert_eq!(read_values, expected);
    });

    writer.join().unwrap();
    reader.join().unwrap();
  }

  #[test]
  fn test_non_sequential_write_single_read() {
    //non_sequential_write_single_read(3, vec![0, 0, 0, 0, 4, 5, 6, 7]);
    //non_sequential_write_single_read(4, (0..8).collect::<Vec<_>>()); // TODO: not critical but would be nice

    //non_sequential_write_single_read(6, vec![0, 0, 2, 3, 4, 5, 6, 7]); // TODO: race condition
    non_sequential_write_single_read(8, (0..8).collect::<Vec<_>>());
  }

  #[test]
  fn test_fixed_hole_single_read() {
    let (mut input, output) = new_owned(16, 0, 6);

    let barrier = Arc::new(Barrier::new(2));
    let barrier_writer = barrier.clone();
    let barrier_reader = barrier.clone();

    let writer = thread::spawn(move || {
      barrier_writer.wait();
      input.write_from_at(0, (0..2).map(|x| x as i32)); // write 0, 1
      input.write_from_at(4, (4..8).map(|x| x as i32)); // write 4, 5, 6, 7
      thread::sleep(std::time::Duration::from_millis(100));
      input.write_from_at(2, (2..4).map(|x| x as i32)); // write 2, 3 to fix the hole
    });

    let reader = thread::spawn(move || {
      barrier_reader.wait();
      let mut read_values = vec![0; 8];
      for i in 0..8 {
        while output.read_at(i, &mut read_values[i..i + 1])
          != (ReadResult { useful_start_index: 0, useful_end_index: 1 })
        {
          thread::yield_now();
        }
      }
      assert_eq!(read_values, (0..8).collect::<Vec<_>>());
    });

    writer.join().unwrap();
    reader.join().unwrap();
  }

  #[test]
  fn test_too_large_hole_single_read() {
    let (mut input, output) = new_owned(16, 0, 3);

    let barrier = Arc::new(Barrier::new(2));
    let barrier_writer = barrier.clone();
    let barrier_reader = barrier.clone();

    let writer = thread::spawn(move || {
      input.write_from_at(0, vec![-1; 8].into_iter());
      input.write_from_at(8, vec![-1; 8].into_iter());
      input.write_from_at(16, (0..2).map(|x| x as i32)); // write 0, 1
      input.write_from_at(16 + 4, (4..8).map(|x| x as i32)); // write 4, 5, 6, 7
      thread::sleep(std::time::Duration::from_millis(100));
      //input.write_from_at(16+8, [].into_iter());
      // TODO: what if the following happens? (data arrives too late and breaks something)
      //input.write_from_at(2, (2..4).map(|x| x as i32)); // write 2, 3 to fix the hole
      barrier_writer.wait();
    });

    let reader = thread::spawn(move || {
      barrier_reader.wait();
      let mut read_values = vec![0; 8];
      for i in 0..8 {
        while output.read_at(i + 16, &mut read_values[i..i + 1])
          != (ReadResult { useful_start_index: 0, useful_end_index: 1 })
        {
          thread::yield_now();
        }
      }
      let mut expected = (0..8).collect::<Vec<_>>();
      expected[2] = 0;
      expected[3] = 0;
      assert_eq!(read_values, expected);
    });

    writer.join().unwrap();
    reader.join().unwrap();
  }

  #[test]
  fn test_write_read() {
    let (mut input, output) = new_owned(16, 0, 4);

    let writer = thread::spawn(move || {
      input.write_from_at(0, (0..8).map(|x| x as i32)); // write 8 items at once
      input.write_from_at(8, (8..16).map(|x| x as i32)); // write another 8 items at once
    });

    let reader = thread::spawn(move || {
      let mut read_values = vec![0; 16];
      while output.read_at(0, &mut read_values[0..8])
        != (ReadResult { useful_start_index: 0, useful_end_index: 8 })
      {
        thread::yield_now();
      }
      while output.read_at(8, &mut read_values[8..16])
        != (ReadResult { useful_start_index: 0, useful_end_index: 8 })
      {
        thread::yield_now();
      }
      assert_eq!(read_values, (0..16).collect::<Vec<_>>());
    });

    writer.join().unwrap();
    reader.join().unwrap();
  }

  #[test]
  fn test_wraparound_separate() {
    let (mut input, output) = new_owned(16, 0, 4);

    let barrier = Arc::new(Barrier::new(2));
    let barrier_writer = barrier.clone();
    let barrier_reader = barrier.clone();

    let writer = thread::spawn(move || {
      barrier_writer.wait();
      input.write_from_at(12, (12..16).map(|x| x as i32)); // write near the end
      input.write_from_at(16, (16..20).map(|x| x as i32)); // wrap around to the beginning
    });

    let reader = thread::spawn(move || {
      barrier_reader.wait();
      let mut read_values = vec![0; 8];
      while output.read_at(12, &mut read_values[0..4])
        != (ReadResult { useful_start_index: 0, useful_end_index: 4 })
      {
        thread::yield_now();
      }
      while output.read_at(16, &mut read_values[4..8])
        != (ReadResult { useful_start_index: 0, useful_end_index: 4 })
      {
        thread::yield_now();
      }
      let expected: Vec<i32> = (12..20).collect();
      assert_eq!(&read_values[0..4], &expected[0..4]);
      assert_eq!(&read_values[4..8], &expected[4..8]);
    });

    writer.join().unwrap();
    reader.join().unwrap();
  }

  #[test]
  fn test_wraparound() {
    let (mut input, output) = new_owned(16, 0, 4);

    let barrier = Arc::new(Barrier::new(2));
    let barrier_writer = barrier.clone();
    let barrier_reader = barrier.clone();

    let writer = thread::spawn(move || {
      barrier_writer.wait();
      input.write_from_at(14, (14..18).map(|x| x as i32)); // write across the boundary
      input.write_from_at(18, (100..102).map(|x| x as i32)); // fix hole 0..14, TODO: handle it properly in write_from_at
    });

    let reader = thread::spawn(move || {
      barrier_reader.wait();
      let mut read_values = vec![0; 4];
      while output.read_at(14, &mut read_values)
        != (ReadResult { useful_start_index: 0, useful_end_index: 4 })
      {
        thread::yield_now();
      }
      let expected: Vec<i32> = (14..18).collect();
      assert_eq!(read_values, expected);
    });

    writer.join().unwrap();
    reader.join().unwrap();
  }

  #[test]
  fn test_wraparound_hole_fix_with_0() {
    let (mut input, output) = new_owned(16, 0, 4);

    let barrier = Arc::new(Barrier::new(2));
    let barrier_writer = barrier.clone();
    let barrier_reader = barrier.clone();

    let writer = thread::spawn(move || {
      barrier_writer.wait();
      input.write_from_at(14, (14..18).map(|x| x as i32)); // write across the boundary
      thread::sleep(std::time::Duration::from_millis(100));
      input.write_from_at(18, (18..22).map(|x| x as i32)); // fix the hole and wrap around the start
    });

    let reader = thread::spawn(move || {
      barrier_reader.wait();
      let mut read_values = vec![0; 10];
      while output.read_at(12, &mut read_values[0..6])
        != (ReadResult { useful_start_index: 0, useful_end_index: 6 })
      {
        thread::yield_now();
      }
      while output.read_at(18, &mut read_values[6..10])
        != (ReadResult { useful_start_index: 0, useful_end_index: 4 })
      {
        thread::yield_now();
      }
      let expected: Vec<i32> = vec![0; 2].into_iter().chain(14..22).collect();
      assert_eq!(read_values, expected);
    });

    writer.join().unwrap();
    reader.join().unwrap();
  }

  #[test]
  fn test_hole_fix_with_0() {
    let (mut input, output) = new_owned(16, 0, 4);

    let barrier = Arc::new(Barrier::new(2));
    let barrier_writer = barrier.clone();
    let barrier_reader = barrier.clone();

    let writer = thread::spawn(move || {
      barrier_writer.wait();
      input.write_from_at(0, (0..4).map(|x| x as i32)); // write some items
      input.write_from_at(8, (8..12).map(|x| x as i32)); // write items creating a hole
      thread::sleep(std::time::Duration::from_millis(200));
      input.write_from_at(12, (12..16).map(|x| x as i32)); // write items after hole_fix_wait
    });

    let reader = thread::spawn(move || {
      barrier_reader.wait();
      let mut read_values = vec![0; 16];
      while output.read_at(0, &mut read_values[0..16])
        != (ReadResult { useful_start_index: 0, useful_end_index: 16 })
      {
        thread::yield_now();
      }
      let expected: Vec<i32> = (0..4)
        .chain(vec![0; 4]) // default values for the hole
        .chain(8..16)
        .collect();
      assert_eq!(read_values, expected);
    });

    writer.join().unwrap();
    reader.join().unwrap();
  }

  #[test]
  fn test_close_items_until_noop_when_behind_readable_pos() {
    let (mut input, output) = new_owned(16, 0, 100);
    input.write_from_at(0, (0..4).map(|x| x as i32));
    assert_eq!(output.readable_until(), 4);
    input.close_items_until(2);
    assert_eq!(output.readable_until(), 4);
    assert_eq!(output.holes_count(), 0);
    let mut read_values = vec![0; 4];
    assert_eq!(output.read_at(0, &mut read_values), ReadResult { useful_start_index: 0, useful_end_index: 4 });
    assert_eq!(read_values, vec![0, 1, 2, 3]);
  }

  #[test]
  fn test_close_items_until_exact_boundary() {
    let (mut input, output) = new_owned(16, 0, 100);
    input.write_from_at(0, (0..4).map(|x| x as i32));
    assert_eq!(output.readable_until(), 4);
    input.close_items_until(4);
    assert_eq!(output.readable_until(), 4);
    assert_eq!(output.holes_count(), 0);
  }

  #[test]
  fn test_close_items_until_future_fill_empty_buffer() {
    let (mut input, output) = new_owned(16, 100, 100);
    let mut read_values = vec![-1; 8];
    assert_eq!(output.read_at(100, &mut read_values), ReadResult { useful_start_index: 0, useful_end_index: 0 });
    input.close_items_until(108);
    assert_eq!(output.readable_until(), 108);
    assert_eq!(output.read_at(100, &mut read_values), ReadResult { useful_start_index: 0, useful_end_index: 8 });
    assert_eq!(read_values, vec![0; 8]);
  }

  #[test]
  fn test_close_items_until_future_fill_past_writes() {
    let (mut input, output) = new_owned(16, 0, 100);
    input.write_from_at(0, (0..4).map(|x| x as i32));
    let mut read_values = vec![-1; 8];
    assert_eq!(output.read_at(4, &mut read_values[4..8]), ReadResult { useful_start_index: 0, useful_end_index: 0 });
    input.close_items_until(8);
    assert_eq!(output.readable_until(), 8);
    assert_eq!(output.read_at(0, &mut read_values), ReadResult { useful_start_index: 0, useful_end_index: 8 });
    assert_eq!(read_values, vec![0, 1, 2, 3, 0, 0, 0, 0]);
  }

  #[test]
  fn test_close_items_until_future_fill_wraparound() {
    let (mut input, output) = new_owned(16, 14, 100);
    let mut read_values = vec![-1; 4];
    assert_eq!(output.read_at(14, &mut read_values), ReadResult { useful_start_index: 0, useful_end_index: 0 });
    input.close_items_until(18);
    assert_eq!(output.readable_until(), 18);
    assert_eq!(output.read_at(14, &mut read_values), ReadResult { useful_start_index: 0, useful_end_index: 4 });
    assert_eq!(read_values, vec![0; 4]);
  }

  #[test]
  fn test_close_items_until_idempotent() {
    let (mut input, output) = new_owned(16, 0, 100);
    input.write_from_at(0, (0..2).map(|x| x as i32));
    input.write_from_at(4, (4..6).map(|x| x as i32));
    assert_eq!(output.readable_until(), 2);
    input.close_items_until(6);
    assert_eq!(output.readable_until(), 6);
    assert_eq!(output.holes_count(), 1);
    input.close_items_until(6);
    assert_eq!(output.readable_until(), 6);
    assert_eq!(output.holes_count(), 1);
    let mut read_values = vec![-1; 6];
    assert_eq!(output.read_at(0, &mut read_values), ReadResult { useful_start_index: 0, useful_end_index: 6 });
    assert_eq!(read_values, vec![0, 1, 0, 0, 4, 5]);
  }

  #[test]
  fn test_close_items_until_after_reset() {
    let (mut input, output) = new_owned(16, 0, 100);
    input.write_from_at(0, (100..108).map(|x| x as i32));
    input.shared().reset(200);
    let mut read_values = vec![-1; 4];
    assert_eq!(output.read_at(200, &mut read_values), ReadResult { useful_start_index: 0, useful_end_index: 0 });
    input.close_items_until(204);
    assert_eq!(output.readable_until(), 204);
    assert_eq!(output.read_at(200, &mut read_values), ReadResult { useful_start_index: 0, useful_end_index: 4 });
    assert_eq!(read_values, vec![0; 4]);
  }

  #[test]
  fn test_close_items_until_usize_wraparound() {
    let (mut input, output) = new_owned(16, usize::MAX - 2, 100);
    let mut read_values = vec![-1; 4];
    assert_eq!(
      output.read_at(usize::MAX - 2, &mut read_values),
      ReadResult { useful_start_index: 0, useful_end_index: 0 }
    );
    input.close_items_until((usize::MAX - 2).wrapping_add(4));
    assert_eq!(output.readable_until(), (usize::MAX - 2).wrapping_add(4));
    assert_eq!(
      output.read_at(usize::MAX - 2, &mut read_values),
      ReadResult { useful_start_index: 0, useful_end_index: 4 }
    );
    assert_eq!(read_values, vec![0; 4]);
  }

  #[test]
  fn test_close_items_until_mixed_ready_unready() {
    let (mut input, output) = new_owned(16, 0, 100);
    input.write_from_at(0, (0..2).map(|x| x as i32));
    input.write_from_at(4, (4..6).map(|x| x as i32));
    assert_eq!(output.readable_until(), 2);
    input.close_items_until(6);
    assert_eq!(output.readable_until(), 6);
    assert_eq!(output.holes_count(), 1);
    let mut read_values = vec![-1; 6];
    assert_eq!(output.read_at(0, &mut read_values), ReadResult { useful_start_index: 0, useful_end_index: 6 });
    assert_eq!(read_values, vec![0, 1, 0, 0, 4, 5]);
  }

  #[test]
  fn test_close_items_until_consecutive_increasing() {
    let (mut input, output) = new_owned(16, 0, 100);
    input.write_from_at(0, (0..2).map(|x| x as i32));
    input.write_from_at(6, (6..8).map(|x| x as i32));
    assert_eq!(output.readable_until(), 2);
    input.close_items_until(4);
    assert_eq!(output.readable_until(), 4);
    assert_eq!(output.holes_count(), 1);
    input.close_items_until(6);
    assert_eq!(output.readable_until(), 6);
    assert_eq!(output.holes_count(), 2);
    input.close_items_until(8);
    assert_eq!(output.readable_until(), 8);
    assert_eq!(output.holes_count(), 2);
    let mut read_values = vec![-1; 8];
    assert_eq!(output.read_at(0, &mut read_values), ReadResult { useful_start_index: 0, useful_end_index: 8 });
    assert_eq!(read_values, vec![0, 1, 0, 0, 0, 0, 6, 7]);
  }

  #[test]
  fn test_close_items_until_does_not_zero_already_ready_items() {
    let (mut input, output) = new_owned(16, 0, 100);

    // Simulate an active audio stream: every slot is written and ready.
    input.write_from_at(0, (100..108).map(|x| x as i32));
    input.write_from_at(8, (108..116).map(|x| x as i32));
    assert_eq!(output.readable_until(), 16);

    // On disconnect SilenceWriter starts calling close_items_until.
    // It future-fills a bit, but existing ready samples stay as-is.
    input.close_items_until(20);

    assert_eq!(output.readable_until(), 20);
    // Positions 16..19 were future-filled with zeros.
    let mut read_future = vec![-1; 4];
    assert_eq!(
      output.read_at(16, &mut read_future),
      ReadResult { useful_start_index: 0, useful_end_index: 4 }
    );
    assert_eq!(read_future, vec![0; 4]);

    // The buffer slots behind readable_pos still hold the original audio.
    // If the consumer reads near the current position (within items_size),
    // it gets a mix of stale audio + silence — this is the corruption
    // reported in issue #41.
    let mut read_tail = vec![-1; 8];
    assert_eq!(
      output.read_at(12, &mut read_tail),
      ReadResult { useful_start_index: 0, useful_end_index: 8 }
    );
    // positions 12..15 = original audio, 16..19 = zeros (future-filled)
    assert_eq!(read_tail, vec![112, 113, 114, 115, 0, 0, 0, 0]);
  }

  /// Write via the ring_buffer API into an ExternalBuffer, then read the
  /// underlying Vec directly (no ring_buffer read types involved).
  /// Covers both the "If closing not-yet-touched items was requested" block
  /// and wrap-around indexing of the future-fill path.
  #[test]
  fn test_external_buffer_close_items_until_future_fill_and_rotation() {
    use atomic::Atomic;
    use std::sync::atomic::Ordering;

    let buf: Arc<Vec<Atomic<i32>>> = Arc::new((0..16).map(|_| Atomic::new(-1)).collect());
    std::mem::forget(buf.clone());

    let valid = Arc::new(std::sync::RwLock::new(true));
    let params = unsafe {
      ExternalBufferParameters::new(buf.as_ptr(), buf.len(), 1, valid.clone(), None)
    };

    let mut input = wrap_external_sink(&params, 12, 100);
    let output = RBOutput { rb: input.shared().clone() };

    // Write samples 14..18 (wraparound)
    input.write_from_at(14, (100..104).map(|x| x as i32));
    
    // There must be a hole 12..14, but not counted yet since we haven't closed it
    assert_eq!(output.holes_count(), 0);
    assert_eq!(output.readable_until(), 12);
    assert_eq!(buf[12].load(Ordering::Relaxed), -1);
    assert_eq!(buf[13].load(Ordering::Relaxed), -1);
    
    
    // Close the hole
    // (actually, close items a bit before writing_pos to verify that it detects the hole already)
    input.close_items_until(16);
    
    assert_eq!(output.holes_count(), 1);
    assert_eq!(buf[12].load(Ordering::Relaxed), 0);
    assert_eq!(buf[13].load(Ordering::Relaxed), 0);
    
    let last_holes_count = output.holes_count();
    
    input.close_items_until(18);
    assert_eq!(output.readable_until(), 18);
    assert_eq!(output.holes_count(), last_holes_count);

    // read the underlying Vec directly, no ring_buffer types used
    assert_eq!(buf[14].load(Ordering::Relaxed), 100);
    assert_eq!(buf[15].load(Ordering::Relaxed), 101);
    assert_eq!(buf[0].load(Ordering::Relaxed), 102);
    assert_eq!(buf[1].load(Ordering::Relaxed), 103);
    for i in 2..12 {
      assert_eq!(buf[i].load(Ordering::Relaxed), -1);
    }

    // Future-fill past writing_pos; [16, 20) wraps to ring indices [0, 4).
    input.close_items_until(20);

    assert_eq!(output.readable_until(), 20);
    assert_eq!(output.holes_count(), last_holes_count);

    // Wrapped future-fill should have zeroed buf[0..4].
    assert_eq!(buf[2].load(Ordering::Relaxed), 0);
    assert_eq!(buf[3].load(Ordering::Relaxed), 0);

    // Positions 14..18 must stay untouched.
    assert_eq!(buf[14].load(Ordering::Relaxed), 100);
    assert_eq!(buf[15].load(Ordering::Relaxed), 101);
    assert_eq!(buf[0].load(Ordering::Relaxed), 102);
    assert_eq!(buf[1].load(Ordering::Relaxed), 103);

    // 12..14 contain the initial hole, already fixed
    assert_eq!(buf[12].load(Ordering::Relaxed), 0);
    assert_eq!(buf[13].load(Ordering::Relaxed), 0);
    
    // Everything else still carries the initial sentinel.
    for i in 4..12 {
      assert_eq!(buf[i].load(Ordering::Relaxed), -1);
    }
  }

  /// Simulates what happens if flows_rx calls close_items_until(ts + latency)
  /// *before* the actual network packet arrives.
  #[test]
  fn test_close_items_until_before_packet_arrives() {
    // Use a 32-item buffer so that reading 18 samples from position 0
    // does not trigger the "lagging behind > items_size" guard in read_at.
    let (mut input, output) = new_owned(32, 0, 100);

    // Phase 1: Timer fires ahead of the packet stream.
    // Future-fills [0, 8) with zeros and advances writing_pos to 8.
    input.close_items_until(8);
    assert_eq!(output.readable_until(), 8);
    assert_eq!(output.holes_count(), 0);

    // Case A — whole packet BEFORE the closed position.
    // A late/backlogged packet writes to [0, 4), entirely inside the
    // future-filled range.  It overwrites its own slots but leaves the
    // tail [4, 8) as zeros.
    input.write_from_at(0, (42..46).map(|x| x as i32));
    assert_eq!(output.readable_until(), 8);

    let mut buf = vec![-1; 8];
    assert_eq!(output.read_at(0, &mut buf), ReadResult { useful_start_index: 0, useful_end_index: 8 });
    assert_eq!(buf, vec![42, 43, 44, 45, 0, 0, 0, 0]);

    // Case B — closed position INSIDE the packet time range.
    // Packet [4, 10) straddles the boundary: [4, 8) overwrites zeros,
    // [8, 10) is newly written.  writing_pos advances to 10.
    input.write_from_at(4, (50..56).map(|x| x as i32));
    assert_eq!(output.readable_until(), 10);

    let mut buf2 = vec![-1; 10];
    assert_eq!(output.read_at(0, &mut buf2), ReadResult { useful_start_index: 0, useful_end_index: 10 });
    assert_eq!(buf2, vec![42, 43, 44, 45, 50, 51, 52, 53, 54, 55]);

    // Case C — whole packet AFTER the closed position, creating a hole.
    // Packet [14, 18) arrives with a gap [10, 14).
    input.write_from_at(14, (70..74).map(|x| x as i32));
    // readable_pos is stuck at 10 because of the hole.
    assert_eq!(output.readable_until(), 10);

    // A subsequent close_items_until fixes the hole [10, 14) with zeros
    // and advances readable_pos to 18.
    input.close_items_until(18);
    assert_eq!(output.readable_until(), 18);
    assert_eq!(output.holes_count(), 1);

    let mut buf3 = vec![-1; 18];
    assert_eq!(output.read_at(0, &mut buf3), ReadResult { useful_start_index: 0, useful_end_index: 18 });
    let expected: Vec<i32> = vec![42, 43, 44, 45, 50, 51, 52, 53, 54, 55]
      .into_iter()
      .chain(vec![0; 4])
      .chain(70..74)
      .collect();
    assert_eq!(buf3, expected);
  }
}

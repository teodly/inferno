use crate::common::*;
use std::{marker::PhantomData, slice, sync::{atomic::AtomicUsize, Arc, RwLock}};
use atomic::{Atomic, Ordering};
use bool_vec::{boolvec, BoolVec};
use bytemuck::NoUninit;
use itertools::Itertools;

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

pub trait ProxyToSamplesBuffer: ProxyToBuffer<Atomic<Sample>> {
}

/// A buffer which owns its data, stored as a `Vec``
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

impl ProxyToSamplesBuffer for OwnedBuffer<Atomic<Sample>> {
}

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

impl ProxyToSamplesBuffer for ExternalBuffer<Atomic<Sample>> {
}

struct RingBufferShared<T, P: ProxyToBuffer<Atomic<T>>> {
  _t: PhantomData<T>,
  buffer: P,
  stride: usize,
  items_size: usize,
  read_pos: AtomicUsize, // TODO remove
  readable_pos: AtomicUsize,
  writing_pos: AtomicUsize,
  holes_count: AtomicUsize,
}

impl<T, P: ProxyToBuffer<Atomic<T>>> RingBufferShared<T, P> {
  fn new(storage: P, stride: usize, start_time: usize) -> Arc<Self> {
    let items_size = (storage.len()+stride-1) / stride;
    Arc::new(RingBufferShared {
      _t: Default::default(),
      buffer: storage,
      stride,
      items_size,
      read_pos: start_time.into(),
      readable_pos: start_time.into(),
      writing_pos: start_time.into(),
      holes_count: 0.into()
    })
  }
  fn item_to_buffer_index(&self, i: usize) -> usize {
    // TODO change to more optimized log2 implementation when we're sure that buffer length will always be power of 2
    (i % self.items_size) * self.stride
  }
}

#[inline(always)]
fn for_in_ring(length: usize, start: usize, end: usize, mut cb: impl FnMut(usize)) {
  if start==end {
    return;
  }
  let w_start = start % length;
  let w_end = end % length;
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

pub struct RBInput<T, P: ProxyToBuffer<Atomic<T>>> {
  rb: Arc<RingBufferShared<T, P>>,
  item_ready: BoolVec,
  hole_fix_wait: usize,
}

impl<T: Default + NoUninit, P: ProxyToBuffer<Atomic<T>>> RBInput<T, P> {
  pub fn write_from_at(&mut self, start_timestamp: usize, mut input: impl ExactSizeIterator<Item = T>) {
    let input_len = input.len();
    assert!(input_len < self.rb.items_size);
    let hole = wrapsub(start_timestamp, self.rb.writing_pos.load(Ordering::Relaxed)) > 0;
    //let wrapped_start_ts = start_timestamp % self.rb.items_size;
    if hole {
      //let wrapped_writing_pos = self.rb.writing_pos.load(Ordering::Relaxed) % self.rb.items_size;
      for_in_ring(self.rb.items_size, self.rb.writing_pos.load(Ordering::Relaxed), start_timestamp, |i| {
        self.item_ready.set(i, false);
      });
    }
    // FIXME wrapping_add % items_size may result in unexpected behaviour if items_size is not power of 2
    // TODO ensure that items_size is power of 2
    let end_ts = start_timestamp.wrapping_add(input_len);
    //let wrapped_end_ts = end_ts % self.rb.items_size;

    self.rb.buffer.map(|buffer| {
      let mut hole_in_past = self.rb.readable_pos.load(Ordering::Relaxed) != self.rb.writing_pos.load(Ordering::Relaxed);

      if wrapsub(end_ts, self.rb.writing_pos.load(Ordering::Relaxed)) > 0 {
        self.rb.writing_pos.store(end_ts, Ordering::Release);
      }
      for_in_ring(self.rb.items_size, start_timestamp, end_ts, |i| {
        //debug!("writing to RB index {i}");
        buffer[i * self.rb.stride].store(input.next().unwrap(), Ordering::Relaxed);
        self.item_ready.set(i, true);
      });
      atomic::fence(Ordering::Release);

      if hole_in_past {
        if self.rb.writing_pos.load(Ordering::Relaxed).wrapping_sub(self.rb.readable_pos.load(Ordering::Relaxed)) > self.hole_fix_wait {
          self.rb.holes_count.fetch_add(1, Ordering::Release);
          let new_readable_pos = self.rb.writing_pos.load(Ordering::Relaxed).wrapping_sub(self.hole_fix_wait);
          for_in_ring(self.rb.items_size, self.rb.readable_pos.load(Ordering::Relaxed), new_readable_pos, |i| {
            if !self.item_ready.get(i).unwrap() {
              buffer[i * self.rb.stride].store(T::default(), Ordering::Relaxed);
            }
          });
          atomic::fence(Ordering::Release);
          self.rb.readable_pos.store(new_readable_pos, Ordering::Release);
        }
        //let wrapped_readable_pos = self.rb.readable_pos.load(Ordering::Relaxed) % self.rb.items_size;
        //let wrapped_writing_pos = self.rb.writing_pos.load(Ordering::Relaxed) % self.rb.items_size;
        let mut ready = true;
        let mut new_write_pos = self.rb.readable_pos.load(Ordering::Relaxed);
        for_in_ring(self.rb.items_size, self.rb.readable_pos.load(Ordering::Relaxed), self.rb.writing_pos.load(Ordering::Relaxed), |i| {
          if !ready { return; }
          if self.item_ready.get(i).unwrap() {
            new_write_pos += 1;
          } else {
            ready = false;
          }
        });
        if ready {
          hole_in_past = false;
        }
        self.rb.readable_pos.store(new_write_pos, Ordering::Release);
      }
      let new_writing_pos = start_timestamp.wrapping_add(input_len);
      if wrapsub(new_writing_pos, self.rb.writing_pos.load(Ordering::Relaxed) /* really? */) > 0 {
        self.rb.writing_pos.store(new_writing_pos, Ordering::Release);
      }
      if !(hole || hole_in_past) {
        // inform the reader(s) that new data is readable
        self.rb.readable_pos.store(self.rb.writing_pos.load(Ordering::Relaxed), Ordering::Release);
      }
    });
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

    if self.rb.buffer.map(|buffer| {
      atomic::fence(Ordering::Acquire);
      for_in_ring(self.rb.items_size, start_timestamp, start_timestamp.wrapping_add(output_len), |rb_index| {
        //debug!("reading from RB index {rb_index}");
        output[out_index] = buffer[rb_index * self.rb.stride].load(Ordering::Relaxed);
        out_index += 1;
      });
    }).is_none() {
      return ReadResult { useful_start_index: 0, useful_end_index: 0 };
    }

    if !self.rb.buffer.unconditional_read() {
      let writing_pos = self.rb.writing_pos.load(Ordering::Acquire);
      let diff = wrapsub(writing_pos, start_timestamp);
      if diff > self.rb.items_size.try_into().unwrap() {
        // data has been overwritten in the meantime, so some data at the beginning of the buffer may be wrong
        let overwritten = diff as usize - self.rb.items_size;
        return ReadResult { useful_start_index: overwritten.min(output_len), useful_end_index: output_len };
      }
    }

    ReadResult { useful_start_index: 0, useful_end_index: output_len }
  }

  pub fn read_done(&self, until: usize) {
    self.rb.read_pos.store(until, Ordering::Release);
  }
  pub fn holes_count(&self) -> usize {
    self.rb.holes_count.load(Ordering::Acquire)
  }
}

pub fn new_owned<T: Default>(length: usize, start_time: usize, hole_fix_wait: usize) -> (RBInput<T, OwnedBuffer<Atomic<T>>>, RBOutput<T, OwnedBuffer<Atomic<T>>>) {
  let shared = RingBufferShared::new(OwnedBuffer::<Atomic<T>>::new(length), 1, start_time);
  (
    RBInput{ rb: shared.clone(), item_ready: boolvec![false; shared.items_size], hole_fix_wait },
    RBOutput{ rb: shared }
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

}

// safety: is guaranteed by lockable valid flag
unsafe impl<T> Send for ExternalBufferParameters<T> {}
unsafe impl<T> Sync for ExternalBufferParameters<T> {}


impl<T> ExternalBufferParameters<T> {
  pub unsafe fn new(ptr: *const Atomic<T>, length: usize, stride: usize, valid: Arc<RwLock<bool>>) -> Self {
    Self { ptr, length, stride, valid }
  }
}


// safety: ExternalBufferParameters::new is unsafe so user acknowledges the dangers when creating the `par` struct
pub fn wrap_external_source<T: Default>(par: &ExternalBufferParameters<T>, start_time: usize) -> RBOutput<T, ExternalBuffer<Atomic<T>>> {
  let external = unsafe { ExternalBuffer::<Atomic<T>>::new(par.ptr, par.length, par.valid.clone()) };
  let shared = RingBufferShared::new(external, par.stride, start_time);
  RBOutput{ rb: shared }
}

pub fn wrap_external_sink<T: Default>(par: &ExternalBufferParameters<T>, start_time: usize, hole_fix_wait: usize) -> RBInput<T, ExternalBuffer<Atomic<T>>> {
  let external = unsafe { ExternalBuffer::<Atomic<T>>::new(par.ptr, par.length, par.valid.clone()) };
  let shared = RingBufferShared::new(external, par.stride, start_time);
  RBInput{ rb: shared.clone(), item_ready: boolvec![false; shared.items_size], hole_fix_wait }
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
              while output.read_at(i, &mut read_values[i..i+1]) != (ReadResult{useful_start_index: 0, useful_end_index: 1}) {
                  thread::yield_now();
              }
          }
          assert_eq!(read_values, (0..16).collect::<Vec<_>>());
      });

      writer.join().unwrap();
      reader.join().unwrap();
  }

  #[test]
  fn test_non_sequential_write_single_read() {
      let (mut input, output) = new_owned(16, 0, 4);

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
              while output.read_at(i, &mut read_values[i..i+1]) != (ReadResult{useful_start_index: 0, useful_end_index: 1}) {
                  thread::yield_now();
              }
          }
          assert_eq!(read_values, (0..8).collect::<Vec<_>>());
      });

      writer.join().unwrap();
      reader.join().unwrap();
  }

  #[test]
  fn test_fixed_hole_single_read() {
      let (mut input, output) = new_owned(16, 0, 4);

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
              while output.read_at(i, &mut read_values[i..i+1]) != (ReadResult{useful_start_index: 0, useful_end_index: 1}) {
                  thread::yield_now();
              }
          }
          assert_eq!(read_values, (0..8).collect::<Vec<_>>());
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
          while output.read_at(0, &mut read_values[0..8]) != (ReadResult{useful_start_index: 0, useful_end_index: 8}) {
              thread::yield_now();
          }
          while output.read_at(8, &mut read_values[8..16]) != (ReadResult{useful_start_index: 0, useful_end_index: 8}) {
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
          while output.read_at(12, &mut read_values[0..4]) != (ReadResult{useful_start_index: 0, useful_end_index: 4}) {
              thread::yield_now();
          }
          while output.read_at(16, &mut read_values[4..8]) != (ReadResult{useful_start_index: 0, useful_end_index: 4}) {
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
          while output.read_at(14, &mut read_values) != (ReadResult{useful_start_index: 0, useful_end_index: 4}) {
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
          while output.read_at(12, &mut read_values[0..6]) != (ReadResult{useful_start_index: 0, useful_end_index: 6}) {
              thread::yield_now();
          }
          while output.read_at(18, &mut read_values[6..10]) != (ReadResult{useful_start_index: 0, useful_end_index: 4}) {
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
          while output.read_at(0, &mut read_values[0..16]) != (ReadResult{useful_start_index: 0, useful_end_index: 16}) {
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

}

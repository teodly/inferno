use std::{pin::Pin, thread::JoinHandle};

use futures::Future;


pub fn run_future_in_new_thread(name: impl ToString, future_cb: impl FnOnce() -> Pin<Box<dyn Future<Output=()> + 'static>> + Send + 'static) -> JoinHandle<()> {
  let name = name.to_string();
  std::thread::Builder::new().name(name.clone()).spawn(move || {
    tokio::runtime::Builder::new_current_thread()
      .thread_name(name)
      .enable_all()
      .build()
      .unwrap()
      .block_on(future_cb());
  }).unwrap()
}

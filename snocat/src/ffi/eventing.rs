use super::ConcurrentHandleMap;
use futures::Future;
use std::sync::Arc;

#[repr(C)]
pub enum EventCompletionState {
  Complete = 0,
  Failed = 1,
  Panicked = 2,
  Cancelled = 3,
  DispatchFailed = 4,
}

#[derive(Debug)]
pub enum EventingError {
  DispatcherDropped,
  DeserializationFailed(anyhow::Error),
  DispatchFailed,
}

impl std::fmt::Display for EventingError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    std::fmt::Debug::fmt(self, f)
  }
}

impl std::error::Error for EventingError {}

#[repr(transparent)]
#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Debug)]
pub struct EventHandle<
  T: serde::ser::Serialize + Send + 'static,
  E: serde::ser::Serialize + Send + 'static,
>(u64, std::marker::PhantomData<Result<T, E>>);

impl<T: serde::ser::Serialize + Send + 'static, E: serde::ser::Serialize + Send + 'static>
  EventHandle<T, E>
{
  pub fn new(event_id: u64) -> Self {
    Self(event_id, std::marker::PhantomData)
  }

  pub fn raw(&self) -> u64 {
    self.0
  }
}

impl<T: serde::ser::Serialize + Send + 'static, E: serde::ser::Serialize + Send + 'static> Into<u64>
  for EventHandle<T, E>
{
  fn into(self) -> u64 {
    self.raw()
  }
}

/// An `EventRunner` tracks Rust Futures as promises across an FFI
/// The remote calls a local future-providing function with a chosen, arbitrary handle,
/// and the local state machine will post back to the remote upon completion or failure.
pub struct EventRunner {
  rt: tokio::runtime::Handle,
  report_task_completion_callback: extern "C" fn(
    handle: u64,
    state: EventCompletionState,
    json_loc: *const u8,
    json_byte_len: u32,
  ) -> (),
}

impl EventRunner {
  pub fn new(
    rt: tokio::runtime::Handle,
    report_task_completion_callback: extern "C" fn(
      handle: u64,
      state: EventCompletionState,
      json_loc: *const u8,
      json_byte_len: u32,
    ) -> (),
  ) -> Self {
    Self {
      rt,
      report_task_completion_callback,
    }
  }

  pub fn fire_evented<
    T: serde::ser::Serialize + Send + 'static,
    E: serde::ser::Serialize + Send + 'static,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
  >(
    &self,
    event_id: u64,
    event_dispatch: Fut,
  ) -> Result<(), EventingError> {
    let report = self.report_task_completion_callback;
    let event_task = self.rt.spawn(async move {
      let res = event_dispatch.await;
      let (json, completion_state) = match &res {
        Ok(success) => (
          serde_json::to_string(success),
          EventCompletionState::Complete,
        ),
        Err(failure) => (serde_json::to_string(failure), EventCompletionState::Failed),
      };
      let json = json.expect("Result serialization must be infallible");
      report(event_id, completion_state, json.as_ptr(), json.len() as u32);
    });

    let monitor = self.monitor(event_id, event_task);
    let _ = self.rt.spawn(monitor);
    Ok(())
  }

  fn monitor(
    &self,
    event_id: u64,
    spawned_task: tokio::task::JoinHandle<()>,
  ) -> impl Future<Output = ()> {
    let report = self.report_task_completion_callback;
    async move {
      if let Err(e) = spawned_task.await {
        let state = if e.is_panic() {
          tracing::error!(target = "ffi_panic", ?event_id, outward = true);
          EventCompletionState::Panicked
        } else if e.is_cancelled() {
          tracing::error!(target = "ffi_event_cancelled", ?event_id, outward = true, error = ?e);
          EventCompletionState::Cancelled
        } else {
          tracing::error!(target = "ffi_event_failure", ?event_id, outward = true, error = ?e);
          EventCompletionState::DispatchFailed
        };
        // Inform the remote that the call failed
        report(event_id, state, 0 as *const u8, 0);
      }
    }
  }

  pub fn fire_evented_handle<
    T: serde::ser::Serialize + Send + 'static,
    E: serde::ser::Serialize + Send + 'static,
    Fut: Future<Output = Result<T, E>> + Send + 'static,
  >(
    &self,
    event_id: EventHandle<T, E>,
    event_dispatch: Fut,
  ) -> Result<(), EventingError> {
    self.fire_evented(event_id.into(), event_dispatch)
  }
}
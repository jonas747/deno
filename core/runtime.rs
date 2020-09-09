// Copyright 2018-2020 the Deno authors. All rights reserved. MIT license.

use rusty_v8 as v8;

use crate::bindings;
use crate::errors::attach_handle_to_error;
use crate::errors::ErrWithV8Handle;
use crate::futures::FutureExt;
use crate::module_specifier::ModuleSpecifier;
use crate::modules::LoadState;
use crate::modules::ModuleId;
use crate::modules::ModuleLoadId;
use crate::modules::ModuleLoader;
use crate::modules::ModuleSource;
use crate::modules::Modules;
use crate::modules::NoopModuleLoader;
use crate::modules::PrepareLoadFuture;
use crate::modules::RecursiveModuleLoad;
use crate::ops::*;
use crate::shared_queue::SharedQueue;
use crate::shared_queue::RECOMMENDED_SIZE;
use crate::ErrBox;
use crate::JsError;
use crate::OpRouter;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use futures::stream::StreamFuture;
use futures::task::AtomicWaker;
use futures::Future;
use std::any::Any;
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::From;
use std::convert::TryFrom;
use std::ffi::c_void;
use std::mem::forget;
use std::ops::Deref;
use std::ops::DerefMut;
use std::option::Option;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Once;
use std::task::Context;
use std::task::Poll;

type PendingOpFuture = Pin<Box<dyn Future<Output = (OpId, Box<[u8]>)>>>;

/// Stores a script used to initialize a Isolate
pub struct Script<'a> {
  pub source: &'a str,
  pub filename: &'a str,
}

// TODO(ry) It's ugly that we have both Script and OwnedScript. Ideally we
// wouldn't expose such twiddly complexity.
struct OwnedScript {
  pub source: String,
  pub filename: String,
}

impl From<Script<'_>> for OwnedScript {
  fn from(s: Script) -> OwnedScript {
    OwnedScript {
      source: s.source.to_string(),
      filename: s.filename.to_string(),
    }
  }
}

pub enum Snapshot {
  Static(&'static [u8]),
  JustCreated(v8::StartupData),
  Boxed(Box<[u8]>),
}

/// Represents data used to initialize an isolate at startup, either
/// in the form of a binary snapshot or a JavaScript source file.
pub enum StartupData<'a> {
  Script(Script<'a>),
  Snapshot(Snapshot),
  None,
}

impl StartupData<'_> {
  fn into_options(self) -> (Option<OwnedScript>, Option<Snapshot>) {
    match self {
      Self::Script(script) => (Some(script.into()), None),
      Self::Snapshot(snapshot) => (None, Some(snapshot)),
      Self::None => (None, None),
    }
  }
}

type JsErrorCreateFn = dyn Fn(JsError) -> ErrBox;

pub type GetErrorClassFn = dyn for<'e> Fn(&'e ErrBox) -> &'static str;

/// Objects that need to live as long as the isolate
#[derive(Default)]
struct IsolateAllocations {
  near_heap_limit_callback_data:
    Option<(Box<RefCell<dyn Any>>, v8::NearHeapLimitCallback)>,
}

/// A single execution context of JavaScript. Corresponds roughly to the "Web
/// Worker" concept in the DOM. A JsRuntime is a Future that can be used with
/// an event loop (Tokio, async_std).
////
/// The JsRuntime future completes when there is an error or when all
/// pending ops have completed.
///
/// Ops are created in JavaScript by calling Deno.core.dispatch(), and in Rust
/// by implementing dispatcher function that takes control buffer and optional zero copy buffer
/// as arguments. An async Op corresponds exactly to a Promise in JavaScript.
pub struct JsRuntime {
  // This is an Option<OwnedIsolate> instead of just OwnedIsolate to workaround
  // an safety issue with SnapshotCreator. See JsRuntime::drop.
  v8_isolate: Option<v8::OwnedIsolate>,
  snapshot_creator: Option<v8::SnapshotCreator>,
  has_snapshotted: bool,
  needs_init: bool,
  startup_script: Option<OwnedScript>,
  allocations: IsolateAllocations,
}

/// Internal state for JsRuntime which is stored in one of v8::Isolate's
/// embedder slots.
pub struct JsRuntimeState {
  pub global_context: Option<v8::Global<v8::Context>>,
  pub(crate) shared_ab: Option<v8::Global<v8::SharedArrayBuffer>>,
  pub(crate) js_recv_cb: Option<v8::Global<v8::Function>>,
  pub(crate) js_macrotask_cb: Option<v8::Global<v8::Function>>,
  pub(crate) pending_promise_exceptions: HashMap<i32, v8::Global<v8::Value>>,
  pub(crate) js_error_create_fn: Box<JsErrorCreateFn>,
  pub(crate) shared: SharedQueue,
  pub(crate) pending_ops: FuturesUnordered<PendingOpFuture>,
  pub(crate) pending_unref_ops: FuturesUnordered<PendingOpFuture>,
  pub(crate) have_unpolled_ops: Cell<bool>,
  pub(crate) op_router: Rc<dyn OpRouter>,
  loader: Rc<dyn ModuleLoader>,
  pub modules: Modules,
  pub(crate) dyn_import_map:
    HashMap<ModuleLoadId, v8::Global<v8::PromiseResolver>>,
  preparing_dyn_imports: FuturesUnordered<Pin<Box<PrepareLoadFuture>>>,
  pending_dyn_imports: FuturesUnordered<StreamFuture<RecursiveModuleLoad>>,
  waker: AtomicWaker,
}

impl Deref for JsRuntime {
  type Target = v8::Isolate;
  fn deref(&self) -> &v8::Isolate {
    self.v8_isolate.as_ref().unwrap()
  }
}

impl DerefMut for JsRuntime {
  fn deref_mut(&mut self) -> &mut v8::Isolate {
    self.v8_isolate.as_mut().unwrap()
  }
}

impl Drop for JsRuntime {
  fn drop(&mut self) {
    if let Some(creator) = self.snapshot_creator.take() {
      // TODO(ry): in rusty_v8, `SnapShotCreator::get_owned_isolate()` returns
      // a `struct OwnedIsolate` which is not actually owned, hence the need
      // here to leak the `OwnedIsolate` in order to avoid a double free and
      // the segfault that it causes.
      let v8_isolate = self.v8_isolate.take().unwrap();
      forget(v8_isolate);

      // TODO(ry) V8 has a strange assert which prevents a SnapshotCreator from
      // being deallocated if it hasn't created a snapshot yet.
      // https://github.com/v8/v8/blob/73212783fbd534fac76cc4b66aac899c13f71fc8/src/api.cc#L603
      // If that assert is removed, this if guard could be removed.
      // WARNING: There may be false positive LSAN errors here.
      if self.has_snapshotted {
        drop(creator);
      }
    }
  }
}

#[allow(clippy::missing_safety_doc)]
pub unsafe fn v8_init() {
  let platform = v8::new_default_platform().unwrap();
  v8::V8::initialize_platform(platform);
  v8::V8::initialize();
  // TODO(ry) This makes WASM compile synchronously. Eventually we should
  // remove this to make it work asynchronously too. But that requires getting
  // PumpMessageLoop and RunMicrotasks setup correctly.
  // See https://github.com/denoland/deno/issues/2544
  let argv = vec![
    "".to_string(),
    "--wasm-test-streaming".to_string(),
    "--no-wasm-async-compilation".to_string(),
    "--harmony-top-level-await".to_string(),
    "--experimental-wasm-bigint".to_string(),
  ];
  v8::V8::set_flags_from_command_line(argv);
}

/// Minimum and maximum bytes of heap used in an isolate
pub struct HeapLimits {
  /// By default V8 starts with a small heap and dynamically grows it to match
  /// the set of live objects. This may lead to ineffective garbage collections
  /// at startup if the live set is large. Setting the initial heap size avoids
  /// such garbage collections. Note that this does not affect young generation
  /// garbage collections.
  pub initial: usize,
  /// When the heap size approaches `max`, V8 will perform series of
  /// garbage collections and invoke the
  /// [NearHeapLimitCallback](TODO).
  /// If the garbage collections do not help and the callback does not
  /// increase the limit, then V8 will crash with V8::FatalProcessOutOfMemory.
  pub max: usize,
}

pub(crate) struct IsolateOptions {
  loader: Rc<dyn ModuleLoader>,
  op_router: Rc<dyn OpRouter>,
  startup_script: Option<OwnedScript>,
  startup_snapshot: Option<Snapshot>,
  will_snapshot: bool,
  heap_limits: Option<HeapLimits>,
}

impl JsRuntime {
  /// startup_data defines the snapshot or script used at startup to initialize
  /// the isolate.
  pub fn new(
    op_router: Rc<dyn OpRouter>,
    startup_data: StartupData,
    will_snapshot: bool,
  ) -> Self {
    let (startup_script, startup_snapshot) = startup_data.into_options();
    let options = IsolateOptions {
      loader: Rc::new(NoopModuleLoader),
      op_router,
      startup_script,
      startup_snapshot,
      will_snapshot,
      heap_limits: None,
    };

    Self::from_options(options)
  }

  // TODO(bartlomieju): add `new_with_loader_and_heap_limits` function?
  /// Create new isolate that can load and execute ESModules.
  pub fn new_with_loader(
    loader: Rc<dyn ModuleLoader>,
    op_router: Rc<dyn OpRouter>,
    startup_data: StartupData,
    will_snapshot: bool,
  ) -> Self {
    let (startup_script, startup_snapshot) = startup_data.into_options();
    let options = IsolateOptions {
      loader,
      op_router,
      startup_script,
      startup_snapshot,
      will_snapshot,
      heap_limits: None,
    };

    Self::from_options(options)
  }

  /// This is useful for controlling memory usage of scripts.
  ///
  /// See [`HeapLimits`](struct.HeapLimits.html) for more details.
  ///
  /// Make sure to use [`add_near_heap_limit_callback`](#method.add_near_heap_limit_callback)
  /// to prevent v8 from crashing when reaching the upper limit.
  pub fn with_heap_limits(
    op_router: Rc<dyn OpRouter>,
    startup_data: StartupData,
    heap_limits: HeapLimits,
  ) -> Self {
    let (startup_script, startup_snapshot) = startup_data.into_options();
    let options = IsolateOptions {
      loader: Rc::new(NoopModuleLoader),
      op_router,
      startup_script,
      startup_snapshot,
      will_snapshot: false,
      heap_limits: Some(heap_limits),
    };

    Self::from_options(options)
  }

  fn from_options(options: IsolateOptions) -> Self {
    static DENO_INIT: Once = Once::new();
    DENO_INIT.call_once(|| {
      unsafe { v8_init() };
    });

    let global_context;
    let (mut isolate, maybe_snapshot_creator) = if options.will_snapshot {
      // TODO(ry) Support loading snapshots before snapshotting.
      assert!(options.startup_snapshot.is_none());
      let mut creator =
        v8::SnapshotCreator::new(Some(&bindings::EXTERNAL_REFERENCES));
      let isolate = unsafe { creator.get_owned_isolate() };
      let mut isolate = JsRuntime::setup_isolate(isolate);
      {
        let scope = &mut v8::HandleScope::new(&mut isolate);
        let context = bindings::initialize_context(scope);
        global_context = v8::Global::new(scope, context);
        creator.set_default_context(context);
      }
      (isolate, Some(creator))
    } else {
      let mut params = v8::Isolate::create_params()
        .external_references(&**bindings::EXTERNAL_REFERENCES);
      let snapshot_loaded = if let Some(snapshot) = options.startup_snapshot {
        params = match snapshot {
          Snapshot::Static(data) => params.snapshot_blob(data),
          Snapshot::JustCreated(data) => params.snapshot_blob(data),
          Snapshot::Boxed(data) => params.snapshot_blob(data),
        };
        true
      } else {
        false
      };

      if let Some(heap_limits) = options.heap_limits {
        params = params.heap_limits(heap_limits.initial, heap_limits.max)
      }

      let isolate = v8::Isolate::new(params);
      let mut isolate = JsRuntime::setup_isolate(isolate);
      {
        let scope = &mut v8::HandleScope::new(&mut isolate);
        let context = if snapshot_loaded {
          v8::Context::new(scope)
        } else {
          // If no snapshot is provided, we initialize the context with empty
          // main source code and source maps.
          bindings::initialize_context(scope)
        };
        global_context = v8::Global::new(scope, context);
      }
      (isolate, None)
    };

    isolate.set_slot(Rc::new(RefCell::new(JsRuntimeState {
      global_context: Some(global_context),
      pending_promise_exceptions: HashMap::new(),
      shared_ab: None,
      js_recv_cb: None,
      js_macrotask_cb: None,
      js_error_create_fn: Box::new(JsError::create),
      shared: SharedQueue::new(RECOMMENDED_SIZE),
      pending_ops: FuturesUnordered::new(),
      pending_unref_ops: FuturesUnordered::new(),
      have_unpolled_ops: Cell::new(false),
      op_router: options.op_router,
      modules: Modules::new(),
      loader: options.loader,
      dyn_import_map: HashMap::new(),
      preparing_dyn_imports: FuturesUnordered::new(),
      pending_dyn_imports: FuturesUnordered::new(),
      waker: AtomicWaker::new(),
    })));

    Self {
      v8_isolate: Some(isolate),
      snapshot_creator: maybe_snapshot_creator,
      has_snapshotted: false,
      needs_init: true,
      startup_script: options.startup_script,
      allocations: IsolateAllocations::default(),
    }
  }

  fn setup_isolate(mut isolate: v8::OwnedIsolate) -> v8::OwnedIsolate {
    isolate.set_capture_stack_trace_for_uncaught_exceptions(true, 10);
    isolate.set_promise_reject_callback(bindings::promise_reject_callback);
    isolate.set_host_initialize_import_meta_object_callback(
      bindings::host_initialize_import_meta_object_callback,
    );
    isolate.set_host_import_module_dynamically_callback(
      bindings::host_import_module_dynamically_callback,
    );
    isolate
  }

  pub fn state(isolate: &v8::Isolate) -> Rc<RefCell<JsRuntimeState>> {
    let s = isolate.get_slot::<Rc<RefCell<JsRuntimeState>>>().unwrap();
    s.clone()
  }

  /// Executes a bit of built-in JavaScript to provide Deno.sharedQueue.
  pub(crate) fn shared_init(&mut self) {
    if self.needs_init {
      self.needs_init = false;
      js_check(self.execute("core.js", include_str!("core.js")));
      // Maybe execute the startup script.
      if let Some(s) = self.startup_script.take() {
        self.execute(&s.filename, &s.source).unwrap()
      }
    }
  }

  /// Executes traditional JavaScript code (traditional = not ES modules)
  ///
  /// ErrBox can be downcast to a type that exposes additional information about
  /// the V8 exception. By default this type is JsError, however it may be a
  /// different type if JsRuntime::set_js_error_create_fn() has been used.
  pub fn execute(
    &mut self,
    js_filename: &str,
    js_source: &str,
  ) -> Result<(), ErrBox> {
    self.shared_init();

    let state_rc = Self::state(self);
    let state = state_rc.borrow();

    let scope = &mut v8::HandleScope::with_context(
      self.v8_isolate.as_mut().unwrap(),
      state.global_context.as_ref().unwrap(),
    );

    drop(state);

    let source = v8::String::new(scope, js_source).unwrap();
    let name = v8::String::new(scope, js_filename).unwrap();
    let origin = bindings::script_origin(scope, name);

    let tc_scope = &mut v8::TryCatch::new(scope);

    let script = match v8::Script::compile(tc_scope, source, Some(&origin)) {
      Some(script) => script,
      None => {
        let exception = tc_scope.exception().unwrap();
        return exception_to_err_result(tc_scope, exception);
      }
    };

    match script.run(tc_scope) {
      Some(_) => Ok(()),
      None => {
        assert!(tc_scope.has_caught());
        let exception = tc_scope.exception().unwrap();
        exception_to_err_result(tc_scope, exception)
      }
    }
  }

  /// Takes a snapshot. The isolate should have been created with will_snapshot
  /// set to true.
  ///
  /// ErrBox can be downcast to a type that exposes additional information about
  /// the V8 exception. By default this type is JsError, however it may be a
  /// different type if JsRuntime::set_js_error_create_fn() has been used.
  pub fn snapshot(&mut self) -> v8::StartupData {
    assert!(self.snapshot_creator.is_some());
    let state = Self::state(self);

    // Note: create_blob() method must not be called from within a HandleScope.
    // TODO(piscisaureus): The rusty_v8 type system should enforce this.
    state.borrow_mut().global_context.take();

    std::mem::take(&mut state.borrow_mut().modules);

    let snapshot_creator = self.snapshot_creator.as_mut().unwrap();
    let snapshot = snapshot_creator
      .create_blob(v8::FunctionCodeHandling::Keep)
      .unwrap();
    self.has_snapshotted = true;

    snapshot
  }

  /// Registers a callback on the isolate when the memory limits are approached.
  /// Use this to prevent V8 from crashing the process when reaching the limit.
  ///
  /// Calls the closure with the current heap limit and the initial heap limit.
  /// The return value of the closure is set as the new limit.
  pub fn add_near_heap_limit_callback<C>(&mut self, cb: C)
  where
    C: FnMut(usize, usize) -> usize + 'static,
  {
    let boxed_cb = Box::new(RefCell::new(cb));
    let data = boxed_cb.as_ptr() as *mut c_void;

    let prev = self
      .allocations
      .near_heap_limit_callback_data
      .replace((boxed_cb, near_heap_limit_callback::<C>));
    if let Some((_, prev_cb)) = prev {
      self
        .v8_isolate
        .as_mut()
        .unwrap()
        .remove_near_heap_limit_callback(prev_cb, 0);
    }

    self
      .v8_isolate
      .as_mut()
      .unwrap()
      .add_near_heap_limit_callback(near_heap_limit_callback::<C>, data);
  }

  pub fn remove_near_heap_limit_callback(&mut self, heap_limit: usize) {
    if let Some((_, cb)) = self.allocations.near_heap_limit_callback_data.take()
    {
      self
        .v8_isolate
        .as_mut()
        .unwrap()
        .remove_near_heap_limit_callback(cb, heap_limit);
    }
  }
}

extern "C" fn near_heap_limit_callback<F>(
  data: *mut c_void,
  current_heap_limit: usize,
  initial_heap_limit: usize,
) -> usize
where
  F: FnMut(usize, usize) -> usize,
{
  let callback = unsafe { &mut *(data as *mut F) };
  callback(current_heap_limit, initial_heap_limit)
}

impl Future for JsRuntime {
  type Output = Result<(), ErrBox>;

  fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
    let runtime = self.get_mut();
    runtime.shared_init();

    let state_rc = Self::state(runtime);
    {
      let state = state_rc.borrow();
      state.waker.register(cx.waker());
    }

    let has_preparing = {
      let state = state_rc.borrow();
      !state.preparing_dyn_imports.is_empty()
    };
    if has_preparing {
      let poll_imports = runtime.prepare_dyn_imports(cx)?;
      assert!(poll_imports.is_ready());
    }

    let has_pending = {
      let state = state_rc.borrow();
      !state.pending_dyn_imports.is_empty()
    };
    if has_pending {
      let poll_imports = runtime.poll_dyn_imports(cx)?;
      assert!(poll_imports.is_ready());
    }

    let scope = &mut v8::HandleScope::with_context(
      &mut **runtime,
      state_rc.borrow().global_context.as_ref().unwrap(),
    );

    check_promise_exceptions(scope)?;

    let mut overflow_response: Option<(OpId, Box<[u8]>)> = None;

    loop {
      let mut state = state_rc.borrow_mut();
      // Now handle actual ops.
      state.have_unpolled_ops.set(false);

      let pending_r = state.pending_ops.poll_next_unpin(cx);
      match pending_r {
        Poll::Ready(None) => break,
        Poll::Pending => break,
        Poll::Ready(Some((op_id, buf))) => {
          let successful_push = state.shared.push(op_id, &buf);
          if !successful_push {
            // If we couldn't push the response to the shared queue, because
            // there wasn't enough size, we will return the buffer via the
            // legacy route, using the argument of deno_respond.
            overflow_response = Some((op_id, buf));
            break;
          }
        }
      };
    }

    loop {
      let mut state = state_rc.borrow_mut();
      let unref_r = state.pending_unref_ops.poll_next_unpin(cx);
      #[allow(clippy::match_wild_err_arm)]
      match unref_r {
        Poll::Ready(None) => break,
        Poll::Pending => break,
        Poll::Ready(Some((op_id, buf))) => {
          let successful_push = state.shared.push(op_id, &buf);
          if !successful_push {
            // If we couldn't push the response to the shared queue, because
            // there wasn't enough size, we will return the buffer via the
            // legacy route, using the argument of deno_respond.
            overflow_response = Some((op_id, buf));
            break;
          }
        }
      };
    }

    {
      let state = state_rc.borrow();
      if state.shared.size() > 0 {
        drop(state);
        async_op_response(scope, None)?;
        // The other side should have shifted off all the messages.
        let state = state_rc.borrow();
        assert_eq!(state.shared.size(), 0);
      }
    }

    {
      if let Some((op_id, buf)) = overflow_response.take() {
        async_op_response(scope, Some((op_id, buf)))?;
      }

      drain_macrotasks(scope)?;

      check_promise_exceptions(scope)?;
    }

    let state = state_rc.borrow();
    // We're idle if pending_ops is empty.
    if state.pending_ops.is_empty()
      && state.pending_dyn_imports.is_empty()
      && state.preparing_dyn_imports.is_empty()
    {
      Poll::Ready(Ok(()))
    } else {
      if state.have_unpolled_ops.get() {
        state.waker.wake();
      }
      Poll::Pending
    }
  }
}

impl JsRuntimeState {
  /// Allows a callback to be set whenever a V8 exception is made. This allows
  /// the caller to wrap the JsError into an error. By default this callback
  /// is set to JsError::create.
  pub fn set_js_error_create_fn(
    &mut self,
    f: impl Fn(JsError) -> ErrBox + 'static,
  ) {
    self.js_error_create_fn = Box::new(f);
  }

  // Called by V8 during `Isolate::mod_instantiate`.
  pub fn module_resolve_cb(
    &mut self,
    specifier: &str,
    referrer_id: ModuleId,
  ) -> ModuleId {
    let referrer = self.modules.get_name(referrer_id).unwrap();
    let specifier = self
      .loader
      .resolve(specifier, referrer, false)
      .expect("Module should have been already resolved");
    self.modules.get_id(specifier.as_str()).unwrap_or(0)
  }

  // Called by V8 during `Isolate::mod_instantiate`.
  pub fn dyn_import_cb(
    &mut self,
    resolver_handle: v8::Global<v8::PromiseResolver>,
    specifier: &str,
    referrer: &str,
  ) {
    debug!("dyn_import specifier {} referrer {} ", specifier, referrer);

    let load = RecursiveModuleLoad::dynamic_import(
      specifier,
      referrer,
      self.loader.clone(),
    );
    self.dyn_import_map.insert(load.id, resolver_handle);
    self.waker.wake();
    let fut = load.prepare().boxed_local();
    self.preparing_dyn_imports.push(fut);
  }
}

fn async_op_response<'s>(
  scope: &mut v8::HandleScope<'s>,
  maybe_buf: Option<(OpId, Box<[u8]>)>,
) -> Result<(), ErrBox> {
  let context = scope.get_current_context();
  let global: v8::Local<v8::Value> = context.global(scope).into();
  let js_recv_cb = JsRuntime::state(scope)
    .borrow()
    .js_recv_cb
    .as_ref()
    .map(|cb| v8::Local::new(scope, cb))
    .expect("Deno.core.recv has not been called.");

  let tc_scope = &mut v8::TryCatch::new(scope);

  match maybe_buf {
    Some((op_id, buf)) => {
      let op_id: v8::Local<v8::Value> =
        v8::Integer::new(tc_scope, op_id as i32).into();
      let ui8: v8::Local<v8::Value> =
        boxed_slice_to_uint8array(tc_scope, buf).into();
      js_recv_cb.call(tc_scope, global, &[op_id, ui8])
    }
    None => js_recv_cb.call(tc_scope, global, &[]),
  };

  match tc_scope.exception() {
    None => Ok(()),
    Some(exception) => exception_to_err_result(tc_scope, exception),
  }
}

fn drain_macrotasks<'s>(scope: &mut v8::HandleScope<'s>) -> Result<(), ErrBox> {
  let context = scope.get_current_context();
  let global: v8::Local<v8::Value> = context.global(scope).into();

  let js_macrotask_cb = match JsRuntime::state(scope)
    .borrow_mut()
    .js_macrotask_cb
    .as_ref()
  {
    Some(cb) => v8::Local::new(scope, cb),
    None => return Ok(()),
  };

  // Repeatedly invoke macrotask callback until it returns true (done),
  // such that ready microtasks would be automatically run before
  // next macrotask is processed.
  let tc_scope = &mut v8::TryCatch::new(scope);

  loop {
    let is_done = js_macrotask_cb.call(tc_scope, global, &[]);

    if let Some(exception) = tc_scope.exception() {
      return exception_to_err_result(tc_scope, exception);
    }

    let is_done = is_done.unwrap();
    if is_done.is_true() {
      break;
    }
  }

  Ok(())
}

pub(crate) fn exception_to_err_result<'s, T>(
  scope: &mut v8::HandleScope<'s>,
  exception: v8::Local<v8::Value>,
) -> Result<T, ErrBox> {
  // TODO(piscisaureus): in rusty_v8, `is_execution_terminating()` should
  // also be implemented on `struct Isolate`.
  let is_terminating_exception =
    scope.thread_safe_handle().is_execution_terminating();
  let mut exception = exception;

  if is_terminating_exception {
    // TerminateExecution was called. Cancel exception termination so that the
    // exception can be created..
    // TODO(piscisaureus): in rusty_v8, `cancel_terminate_execution()` should
    // also be implemented on `struct Isolate`.
    scope.thread_safe_handle().cancel_terminate_execution();

    // Maybe make a new exception object.
    if exception.is_null_or_undefined() {
      let message = v8::String::new(scope, "execution terminated").unwrap();
      exception = v8::Exception::error(scope, message);
    }
  }

  let js_error = JsError::from_v8_exception(scope, exception);

  let state_rc = JsRuntime::state(scope);
  let state = state_rc.borrow();
  let js_error = (state.js_error_create_fn)(js_error);

  if is_terminating_exception {
    // Re-enable exception termination.
    // TODO(piscisaureus): in rusty_v8, `terminate_execution()` should also
    // be implemented on `struct Isolate`.
    scope.thread_safe_handle().terminate_execution();
  }

  Err(js_error)
}

fn check_promise_exceptions<'s>(
  scope: &mut v8::HandleScope<'s>,
) -> Result<(), ErrBox> {
  let state_rc = JsRuntime::state(scope);
  let mut state = state_rc.borrow_mut();

  if let Some(&key) = state.pending_promise_exceptions.keys().next() {
    let handle = state.pending_promise_exceptions.remove(&key).unwrap();
    drop(state);
    let exception = v8::Local::new(scope, handle);
    exception_to_err_result(scope, exception)
  } else {
    Ok(())
  }
}

pub fn js_check<T>(r: Result<T, ErrBox>) -> T {
  if let Err(e) = r {
    panic!(e.to_string());
  }
  r.unwrap()
}

fn boxed_slice_to_uint8array<'sc>(
  scope: &mut v8::HandleScope<'sc>,
  buf: Box<[u8]>,
) -> v8::Local<'sc, v8::Uint8Array> {
  assert!(!buf.is_empty());
  let buf_len = buf.len();
  let backing_store = v8::ArrayBuffer::new_backing_store_from_boxed_slice(buf);
  let backing_store_shared = backing_store.make_shared();
  let ab = v8::ArrayBuffer::with_backing_store(scope, &backing_store_shared);
  v8::Uint8Array::new(scope, ab, 0, buf_len)
    .expect("Failed to create UintArray8")
}

// Related to module loading
impl JsRuntime {
  /// Low-level module creation.
  ///
  /// Called during module loading or dynamic import loading.
  fn mod_new(
    &mut self,
    main: bool,
    name: &str,
    source: &str,
  ) -> Result<ModuleId, ErrBox> {
    let state_rc = Self::state(self);
    let scope = &mut v8::HandleScope::with_context(
      &mut **self,
      state_rc.borrow().global_context.as_ref().unwrap(),
    );

    let name_str = v8::String::new(scope, name).unwrap();
    let source_str = v8::String::new(scope, source).unwrap();

    let origin = bindings::module_origin(scope, name_str);
    let source = v8::script_compiler::Source::new(source_str, &origin);

    let tc_scope = &mut v8::TryCatch::new(scope);

    let maybe_module = v8::script_compiler::compile_module(tc_scope, source);

    if tc_scope.has_caught() {
      assert!(maybe_module.is_none());
      let e = tc_scope.exception().unwrap();
      return exception_to_err_result(tc_scope, e);
    }

    let module = maybe_module.unwrap();
    let id = module.get_identity_hash();

    let mut import_specifiers: Vec<ModuleSpecifier> = vec![];
    for i in 0..module.get_module_requests_length() {
      let import_specifier =
        module.get_module_request(i).to_rust_string_lossy(tc_scope);
      let state = state_rc.borrow();
      let module_specifier =
        state.loader.resolve(&import_specifier, name, false)?;
      import_specifiers.push(module_specifier);
    }

    state_rc.borrow_mut().modules.register(
      id,
      name,
      main,
      v8::Global::<v8::Module>::new(tc_scope, module),
      import_specifiers,
    );

    Ok(id)
  }

  /// Instantiates a ES module
  ///
  /// ErrBox can be downcast to a type that exposes additional information about
  /// the V8 exception. By default this type is JsError, however it may be a
  /// different type if JsRuntime::set_js_error_create_fn() has been used.
  fn mod_instantiate(&mut self, id: ModuleId) -> Result<(), ErrBox> {
    let state_rc = Self::state(self);
    let state = state_rc.borrow();
    let scope = &mut v8::HandleScope::with_context(
      &mut **self,
      state.global_context.as_ref().unwrap(),
    );
    let tc_scope = &mut v8::TryCatch::new(scope);

    let module = match state.modules.get_info(id) {
      Some(info) => v8::Local::new(tc_scope, &info.handle),
      None if id == 0 => return Ok(()),
      _ => panic!("module id {} not found in module table", id),
    };
    drop(state);

    if module.get_status() == v8::ModuleStatus::Errored {
      exception_to_err_result(tc_scope, module.get_exception())?
    }

    let result =
      module.instantiate_module(tc_scope, bindings::module_resolve_callback);
    match result {
      Some(_) => Ok(()),
      None => {
        let exception = tc_scope.exception().unwrap();
        exception_to_err_result(tc_scope, exception)
      }
    }
  }

  /// Evaluates an already instantiated ES module.
  ///
  /// ErrBox can be downcast to a type that exposes additional information about
  /// the V8 exception. By default this type is JsError, however it may be a
  /// different type if JsRuntime::set_js_error_create_fn() has been used.
  pub fn mod_evaluate(&mut self, id: ModuleId) -> Result<(), ErrBox> {
    self.shared_init();

    let state_rc = Self::state(self);

    let scope = &mut v8::HandleScope::with_context(
      &mut **self,
      state_rc.borrow().global_context.as_ref().unwrap(),
    );

    let module = state_rc
      .borrow()
      .modules
      .get_info(id)
      .map(|info| v8::Local::new(scope, &info.handle))
      .expect("ModuleInfo not found");
    let mut status = module.get_status();

    if status == v8::ModuleStatus::Instantiated {
      // IMPORTANT: Top-level-await is enabled, which means that return value
      // of module evaluation is a promise.
      //
      // Because that promise is created internally by V8, when error occurs during
      // module evaluation the promise is rejected, and since the promise has no rejection
      // handler it will result in call to `bindings::promise_reject_callback` adding
      // the promise to pending promise rejection table - meaning JsRuntime will return
      // error on next poll().
      //
      // This situation is not desirable as we want to manually return error at the
      // end of this function to handle it further. It means we need to manually
      // remove this promise from pending promise rejection table.
      //
      // For more details see:
      // https://github.com/denoland/deno/issues/4908
      // https://v8.dev/features/top-level-await#module-execution-order
      let maybe_value = module.evaluate(scope);

      // Update status after evaluating.
      status = module.get_status();

      if let Some(value) = maybe_value {
        assert!(
          status == v8::ModuleStatus::Evaluated
            || status == v8::ModuleStatus::Errored
        );
        let promise = v8::Local::<v8::Promise>::try_from(value)
          .expect("Expected to get promise as module evaluation result");
        let promise_id = promise.get_identity_hash();
        let mut state = state_rc.borrow_mut();
        state.pending_promise_exceptions.remove(&promise_id);
      } else {
        assert!(status == v8::ModuleStatus::Errored);
      }
    }

    match status {
      v8::ModuleStatus::Evaluated => Ok(()),
      v8::ModuleStatus::Errored => {
        let exception = module.get_exception();
        exception_to_err_result(scope, exception)
          .map_err(|err| attach_handle_to_error(scope, err, exception))
      }
      other => panic!("Unexpected module status {:?}", other),
    }
  }

  fn dyn_import_error(
    &mut self,
    id: ModuleLoadId,
    err: ErrBox,
  ) -> Result<(), ErrBox> {
    let state_rc = Self::state(self);

    let scope = &mut v8::HandleScope::with_context(
      &mut **self,
      state_rc.borrow().global_context.as_ref().unwrap(),
    );

    let resolver_handle = state_rc
      .borrow_mut()
      .dyn_import_map
      .remove(&id)
      .expect("Invalid dyn import id");
    let resolver = resolver_handle.get(scope);

    let exception = err
      .downcast_ref::<ErrWithV8Handle>()
      .map(|err| err.get_handle(scope))
      .unwrap_or_else(|| {
        let message = err.to_string();
        let message = v8::String::new(scope, &message).unwrap();
        v8::Exception::type_error(scope, message)
      });

    resolver.reject(scope, exception).unwrap();
    scope.perform_microtask_checkpoint();
    Ok(())
  }

  fn dyn_import_done(
    &mut self,
    id: ModuleLoadId,
    mod_id: ModuleId,
  ) -> Result<(), ErrBox> {
    let state_rc = Self::state(self);

    debug!("dyn_import_done {} {:?}", id, mod_id);
    assert!(mod_id != 0);
    let scope = &mut v8::HandleScope::with_context(
      &mut **self,
      state_rc.borrow().global_context.as_ref().unwrap(),
    );

    let resolver_handle = state_rc
      .borrow_mut()
      .dyn_import_map
      .remove(&id)
      .expect("Invalid dyn import id");
    let resolver = resolver_handle.get(scope);

    let module = {
      let state = state_rc.borrow();
      state
        .modules
        .get_info(mod_id)
        .map(|info| v8::Local::new(scope, &info.handle))
        .expect("Dyn import module info not found")
    };
    // Resolution success
    assert_eq!(module.get_status(), v8::ModuleStatus::Evaluated);

    let module_namespace = module.get_module_namespace();
    resolver.resolve(scope, module_namespace).unwrap();
    scope.perform_microtask_checkpoint();
    Ok(())
  }

  fn prepare_dyn_imports(
    &mut self,
    cx: &mut Context,
  ) -> Poll<Result<(), ErrBox>> {
    let state_rc = Self::state(self);

    loop {
      let r = {
        let mut state = state_rc.borrow_mut();
        state.preparing_dyn_imports.poll_next_unpin(cx)
      };
      match r {
        Poll::Pending | Poll::Ready(None) => {
          // There are no active dynamic import loaders, or none are ready.
          return Poll::Ready(Ok(()));
        }
        Poll::Ready(Some(prepare_poll)) => {
          let dyn_import_id = prepare_poll.0;
          let prepare_result = prepare_poll.1;

          match prepare_result {
            Ok(load) => {
              let state = state_rc.borrow_mut();
              state.pending_dyn_imports.push(load.into_future());
            }
            Err(err) => {
              self.dyn_import_error(dyn_import_id, err)?;
            }
          }
        }
      }
    }
  }

  fn poll_dyn_imports(&mut self, cx: &mut Context) -> Poll<Result<(), ErrBox>> {
    let state_rc = Self::state(self);
    loop {
      let poll_result = {
        let mut state = state_rc.borrow_mut();
        state.pending_dyn_imports.poll_next_unpin(cx)
      };

      match poll_result {
        Poll::Pending | Poll::Ready(None) => {
          // There are no active dynamic import loaders, or none are ready.
          return Poll::Ready(Ok(()));
        }
        Poll::Ready(Some(load_stream_poll)) => {
          let maybe_result = load_stream_poll.0;
          let mut load = load_stream_poll.1;
          let dyn_import_id = load.id;

          if let Some(load_stream_result) = maybe_result {
            match load_stream_result {
              Ok(info) => {
                // A module (not necessarily the one dynamically imported) has been
                // fetched. Create and register it, and if successful, poll for the
                // next recursive-load event related to this dynamic import.
                match self.register_during_load(info, &mut load) {
                  Ok(()) => {
                    // Keep importing until it's fully drained
                    let state = state_rc.borrow_mut();
                    state.pending_dyn_imports.push(load.into_future());
                  }
                  Err(err) => self.dyn_import_error(dyn_import_id, err)?,
                }
              }
              Err(err) => {
                // A non-javascript error occurred; this could be due to a an invalid
                // module specifier, or a problem with the source map, or a failure
                // to fetch the module source code.
                self.dyn_import_error(dyn_import_id, err)?
              }
            }
          } else {
            // The top-level module from a dynamic import has been instantiated.
            // Load is done.
            let module_id = load.root_module_id.unwrap();
            self.mod_instantiate(module_id)?;
            match self.mod_evaluate(module_id) {
              Ok(()) => self.dyn_import_done(dyn_import_id, module_id)?,
              Err(err) => self.dyn_import_error(dyn_import_id, err)?,
            };
          }
        }
      }
    }
  }

  fn register_during_load(
    &mut self,
    info: ModuleSource,
    load: &mut RecursiveModuleLoad,
  ) -> Result<(), ErrBox> {
    let ModuleSource {
      code,
      module_url_specified,
      module_url_found,
    } = info;

    let is_main =
      load.state == LoadState::LoadingRoot && !load.is_dynamic_import();
    let referrer_specifier =
      ModuleSpecifier::resolve_url(&module_url_found).unwrap();

    let state_rc = Self::state(self);
    // #A There are 3 cases to handle at this moment:
    // 1. Source code resolved result have the same module name as requested
    //    and is not yet registered
    //     -> register
    // 2. Source code resolved result have a different name as requested:
    //   2a. The module with resolved module name has been registered
    //     -> alias
    //   2b. The module with resolved module name has not yet been registered
    //     -> register & alias

    // If necessary, register an alias.
    if module_url_specified != module_url_found {
      let mut state = state_rc.borrow_mut();
      state
        .modules
        .alias(&module_url_specified, &module_url_found);
    }

    let maybe_mod_id = {
      let state = state_rc.borrow();
      state.modules.get_id(&module_url_found)
    };

    let module_id = match maybe_mod_id {
      Some(id) => {
        // Module has already been registered.
        debug!(
          "Already-registered module fetched again: {}",
          module_url_found
        );
        id
      }
      // Module not registered yet, do it now.
      None => self.mod_new(is_main, &module_url_found, &code)?,
    };

    // Now we must iterate over all imports of the module and load them.
    let imports = {
      let state_rc = Self::state(self);
      let state = state_rc.borrow();
      state.modules.get_children(module_id).unwrap().clone()
    };

    for module_specifier in imports {
      let is_registered = {
        let state_rc = Self::state(self);
        let state = state_rc.borrow();
        state.modules.is_registered(&module_specifier)
      };
      if !is_registered {
        load
          .add_import(module_specifier.to_owned(), referrer_specifier.clone());
      }
    }

    // If we just finished loading the root module, store the root module id.
    if load.state == LoadState::LoadingRoot {
      load.root_module_id = Some(module_id);
      load.state = LoadState::LoadingImports;
    }

    if load.pending.is_empty() {
      load.state = LoadState::Done;
    }

    Ok(())
  }

  /// Asynchronously load specified module and all of it's dependencies
  ///
  /// User must call `JsRuntime::mod_evaluate` with returned `ModuleId`
  /// manually after load is finished.
  pub async fn load_module(
    &mut self,
    specifier: &ModuleSpecifier,
    code: Option<String>,
  ) -> Result<ModuleId, ErrBox> {
    self.shared_init();
    let loader = {
      let state_rc = Self::state(self);
      let state = state_rc.borrow();
      state.loader.clone()
    };

    let load = RecursiveModuleLoad::main(&specifier.to_string(), code, loader);
    let (_load_id, prepare_result) = load.prepare().await;

    let mut load = prepare_result?;

    while let Some(info_result) = load.next().await {
      let info = info_result?;
      self.register_during_load(info, &mut load)?;
    }

    let root_id = load.root_module_id.expect("Root module id empty");
    self.mod_instantiate(root_id).map(|_| root_id)
  }
}

#[cfg(test)]
pub mod tests {
  use super::*;
  use crate::modules::ModuleSourceFuture;
  use crate::ops::*;
  use crate::BasicState;
  use crate::BufVec;
  use futures::future::lazy;
  use futures::FutureExt;
  use std::io;
  use std::ops::FnOnce;
  use std::rc::Rc;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;

  pub fn run_in_task<F>(f: F)
  where
    F: FnOnce(&mut Context) + Send + 'static,
  {
    futures::executor::block_on(lazy(move |cx| f(cx)));
  }

  fn poll_until_ready<F>(future: &mut F, max_poll_count: usize) -> F::Output
  where
    F: Future + Unpin,
  {
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    for _ in 0..max_poll_count {
      match future.poll_unpin(&mut cx) {
        Poll::Pending => continue,
        Poll::Ready(val) => return val,
      }
    }
    panic!(
      "JsRuntime still not ready after polling {} times.",
      max_poll_count
    )
  }

  enum Mode {
    Async,
    AsyncUnref,
    AsyncZeroCopy(u8),
    OverflowReqSync,
    OverflowResSync,
    OverflowReqAsync,
    OverflowResAsync,
  }

  struct TestOpRouter {
    mode: Mode,
    dispatch_count: Arc<AtomicUsize>,
  }

  impl OpRouter for TestOpRouter {
    fn route_op(self: Rc<Self>, op_id: OpId, bufs: BufVec) -> Op {
      if op_id != 1 {
        return Op::NotFound;
      }
      self.dispatch_count.fetch_add(1, Ordering::Relaxed);
      match self.mode {
        Mode::Async => {
          assert_eq!(bufs.len(), 1);
          assert_eq!(bufs[0].len(), 1);
          assert_eq!(bufs[0][0], 42);
          let buf = vec![43u8].into_boxed_slice();
          Op::Async(futures::future::ready(buf).boxed())
        }
        Mode::AsyncUnref => {
          assert_eq!(bufs.len(), 1);
          assert_eq!(bufs[0].len(), 1);
          assert_eq!(bufs[0][0], 42);
          let fut = async {
            // This future never finish.
            futures::future::pending::<()>().await;
            vec![43u8].into_boxed_slice()
          };
          Op::AsyncUnref(fut.boxed())
        }
        Mode::AsyncZeroCopy(count) => {
          assert_eq!(bufs.len(), count as usize);
          bufs.iter().enumerate().for_each(|(idx, buf)| {
            assert_eq!(buf.len(), 1);
            assert_eq!(idx, buf[0] as usize);
          });

          let buf = vec![43u8].into_boxed_slice();
          Op::Async(futures::future::ready(buf).boxed())
        }
        Mode::OverflowReqSync => {
          assert_eq!(bufs.len(), 1);
          assert_eq!(bufs[0].len(), 100 * 1024 * 1024);
          let buf = vec![43u8].into_boxed_slice();
          Op::Sync(buf)
        }
        Mode::OverflowResSync => {
          assert_eq!(bufs.len(), 1);
          assert_eq!(bufs[0].len(), 1);
          assert_eq!(bufs[0][0], 42);
          let mut vec = Vec::<u8>::new();
          vec.resize(100 * 1024 * 1024, 0);
          vec[0] = 99;
          let buf = vec.into_boxed_slice();
          Op::Sync(buf)
        }
        Mode::OverflowReqAsync => {
          assert_eq!(bufs.len(), 1);
          assert_eq!(bufs[0].len(), 100 * 1024 * 1024);
          let buf = vec![43u8].into_boxed_slice();
          Op::Async(futures::future::ready(buf).boxed())
        }
        Mode::OverflowResAsync => {
          assert_eq!(bufs.len(), 1);
          assert_eq!(bufs[0].len(), 1);
          assert_eq!(bufs[0][0], 42);
          let mut vec = Vec::<u8>::new();
          vec.resize(100 * 1024 * 1024, 0);
          vec[0] = 4;
          let buf = vec.into_boxed_slice();
          Op::Async(futures::future::ready(buf).boxed())
        }
      }
    }
  }

  fn setup(mode: Mode) -> (JsRuntime, Arc<AtomicUsize>) {
    let dispatch_count = Arc::new(AtomicUsize::new(0));
    let test_state = Rc::new(TestOpRouter {
      mode,
      dispatch_count: dispatch_count.clone(),
    });
    let mut runtime = JsRuntime::new(test_state, StartupData::None, false);

    js_check(runtime.execute(
      "setup.js",
      r#"
        function assert(cond) {
          if (!cond) {
            throw Error("assert");
          }
        }
        "#,
    ));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 0);
    (runtime, dispatch_count)
  }

  #[test]
  fn test_dispatch() {
    let (mut runtime, dispatch_count) = setup(Mode::Async);
    js_check(runtime.execute(
      "filename.js",
      r#"
        let control = new Uint8Array([42]);
        Deno.core.send(1, control);
        async function main() {
          Deno.core.send(1, control);
        }
        main();
        "#,
    ));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 2);
  }

  #[test]
  fn test_dispatch_no_zero_copy_buf() {
    let (mut runtime, dispatch_count) = setup(Mode::AsyncZeroCopy(0));
    js_check(runtime.execute(
      "filename.js",
      r#"
        Deno.core.send(1);
        "#,
    ));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn test_dispatch_stack_zero_copy_bufs() {
    let (mut runtime, dispatch_count) = setup(Mode::AsyncZeroCopy(2));
    js_check(runtime.execute(
      "filename.js",
      r#"
        let zero_copy_a = new Uint8Array([0]);
        let zero_copy_b = new Uint8Array([1]);
        Deno.core.send(1, zero_copy_a, zero_copy_b);
        "#,
    ));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn test_dispatch_heap_zero_copy_bufs() {
    let (mut runtime, dispatch_count) = setup(Mode::AsyncZeroCopy(5));
    js_check(runtime.execute(
      "filename.js",
      r#"
        let zero_copy_a = new Uint8Array([0]);
        let zero_copy_b = new Uint8Array([1]);
        let zero_copy_c = new Uint8Array([2]);
        let zero_copy_d = new Uint8Array([3]);
        let zero_copy_e = new Uint8Array([4]);
        Deno.core.send(1, zero_copy_a, zero_copy_b, zero_copy_c, zero_copy_d, zero_copy_e);
        "#,
    ));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn test_poll_async_delayed_ops() {
    run_in_task(|cx| {
      let (mut runtime, dispatch_count) = setup(Mode::Async);

      js_check(runtime.execute(
        "setup2.js",
        r#"
         let nrecv = 0;
         Deno.core.setAsyncHandler(1, (buf) => {
           nrecv++;
         });
         "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 0);
      js_check(runtime.execute(
        "check1.js",
        r#"
         assert(nrecv == 0);
         let control = new Uint8Array([42]);
         Deno.core.send(1, control);
         assert(nrecv == 0);
         "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
      assert!(matches!(runtime.poll_unpin(cx), Poll::Ready(Ok(_))));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
      js_check(runtime.execute(
        "check2.js",
        r#"
         assert(nrecv == 1);
         Deno.core.send(1, control);
         assert(nrecv == 1);
         "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 2);
      assert!(matches!(runtime.poll_unpin(cx), Poll::Ready(Ok(_))));
      js_check(runtime.execute("check3.js", "assert(nrecv == 2)"));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 2);
      // We are idle, so the next poll should be the last.
      assert!(matches!(runtime.poll_unpin(cx), Poll::Ready(Ok(_))));
    });
  }

  #[test]
  fn test_poll_async_optional_ops() {
    run_in_task(|cx| {
      let (mut runtime, dispatch_count) = setup(Mode::AsyncUnref);
      js_check(runtime.execute(
        "check1.js",
        r#"
          Deno.core.setAsyncHandler(1, (buf) => {
            // This handler will never be called
            assert(false);
          });
          let control = new Uint8Array([42]);
          Deno.core.send(1, control);
        "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
      // The above op never finish, but runtime can finish
      // because the op is an unreffed async op.
      assert!(matches!(runtime.poll_unpin(cx), Poll::Ready(Ok(_))));
    })
  }

  #[test]
  fn terminate_execution() {
    let (mut isolate, _dispatch_count) = setup(Mode::Async);
    // TODO(piscisaureus): in rusty_v8, the `thread_safe_handle()` method
    // should not require a mutable reference to `struct rusty_v8::Isolate`.
    let v8_isolate_handle =
      isolate.v8_isolate.as_mut().unwrap().thread_safe_handle();

    let terminator_thread = std::thread::spawn(move || {
      // allow deno to boot and run
      std::thread::sleep(std::time::Duration::from_millis(100));

      // terminate execution
      let ok = v8_isolate_handle.terminate_execution();
      assert!(ok);
    });

    // Rn an infinite loop, which should be terminated.
    match isolate.execute("infinite_loop.js", "for(;;) {}") {
      Ok(_) => panic!("execution should be terminated"),
      Err(e) => {
        assert_eq!(e.to_string(), "Uncaught Error: execution terminated")
      }
    };

    // Cancel the execution-terminating exception in order to allow script
    // execution again.
    // TODO(piscisaureus): in rusty_v8, `cancel_terminate_execution()` should
    // also be implemented on `struct Isolate`.
    let ok = isolate
      .v8_isolate
      .as_mut()
      .unwrap()
      .thread_safe_handle()
      .cancel_terminate_execution();
    assert!(ok);

    // Verify that the isolate usable again.
    isolate
      .execute("simple.js", "1 + 1")
      .expect("execution should be possible again");

    terminator_thread.join().unwrap();
  }

  #[test]
  fn dangling_shared_isolate() {
    let v8_isolate_handle = {
      // isolate is dropped at the end of this block
      let (mut runtime, _dispatch_count) = setup(Mode::Async);
      // TODO(piscisaureus): in rusty_v8, the `thread_safe_handle()` method
      // should not require a mutable reference to `struct rusty_v8::Isolate`.
      runtime.v8_isolate.as_mut().unwrap().thread_safe_handle()
    };

    // this should not SEGFAULT
    v8_isolate_handle.terminate_execution();
  }

  #[test]
  fn overflow_req_sync() {
    let (mut runtime, dispatch_count) = setup(Mode::OverflowReqSync);
    js_check(runtime.execute(
      "overflow_req_sync.js",
      r#"
        let asyncRecv = 0;
        Deno.core.setAsyncHandler(1, (buf) => { asyncRecv++ });
        // Large message that will overflow the shared space.
        let control = new Uint8Array(100 * 1024 * 1024);
        let response = Deno.core.dispatch(1, control);
        assert(response instanceof Uint8Array);
        assert(response.length == 1);
        assert(response[0] == 43);
        assert(asyncRecv == 0);
        "#,
    ));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn overflow_res_sync() {
    // TODO(ry) This test is quite slow due to memcpy-ing 100MB into JS. We
    // should optimize this.
    let (mut runtime, dispatch_count) = setup(Mode::OverflowResSync);
    js_check(runtime.execute(
      "overflow_res_sync.js",
      r#"
        let asyncRecv = 0;
        Deno.core.setAsyncHandler(1, (buf) => { asyncRecv++ });
        // Large message that will overflow the shared space.
        let control = new Uint8Array([42]);
        let response = Deno.core.dispatch(1, control);
        assert(response instanceof Uint8Array);
        assert(response.length == 100 * 1024 * 1024);
        assert(response[0] == 99);
        assert(asyncRecv == 0);
        "#,
    ));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn overflow_req_async() {
    run_in_task(|cx| {
      let (mut runtime, dispatch_count) = setup(Mode::OverflowReqAsync);
      js_check(runtime.execute(
        "overflow_req_async.js",
        r#"
         let asyncRecv = 0;
         Deno.core.setAsyncHandler(1, (buf) => {
           assert(buf.byteLength === 1);
           assert(buf[0] === 43);
           asyncRecv++;
         });
         // Large message that will overflow the shared space.
         let control = new Uint8Array(100 * 1024 * 1024);
         let response = Deno.core.dispatch(1, control);
         // Async messages always have null response.
         assert(response == null);
         assert(asyncRecv == 0);
         "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
      assert!(matches!(runtime.poll_unpin(cx), Poll::Ready(Ok(_))));
      js_check(runtime.execute("check.js", "assert(asyncRecv == 1);"));
    });
  }

  #[test]
  fn overflow_res_async() {
    run_in_task(|_cx| {
      // TODO(ry) This test is quite slow due to memcpy-ing 100MB into JS. We
      // should optimize this.
      let (mut runtime, dispatch_count) = setup(Mode::OverflowResAsync);
      js_check(runtime.execute(
        "overflow_res_async.js",
        r#"
         let asyncRecv = 0;
         Deno.core.setAsyncHandler(1, (buf) => {
           assert(buf.byteLength === 100 * 1024 * 1024);
           assert(buf[0] === 4);
           asyncRecv++;
         });
         // Large message that will overflow the shared space.
         let control = new Uint8Array([42]);
         let response = Deno.core.dispatch(1, control);
         assert(response == null);
         assert(asyncRecv == 0);
         "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
      poll_until_ready(&mut runtime, 3).unwrap();
      js_check(runtime.execute("check.js", "assert(asyncRecv == 1);"));
    });
  }

  #[test]
  fn overflow_res_multiple_dispatch_async() {
    // TODO(ry) This test is quite slow due to memcpy-ing 100MB into JS. We
    // should optimize this.
    run_in_task(|_cx| {
      let (mut runtime, dispatch_count) = setup(Mode::OverflowResAsync);
      js_check(runtime.execute(
        "overflow_res_multiple_dispatch_async.js",
        r#"
         let asyncRecv = 0;
         Deno.core.setAsyncHandler(1, (buf) => {
           assert(buf.byteLength === 100 * 1024 * 1024);
           assert(buf[0] === 4);
           asyncRecv++;
         });
         // Large message that will overflow the shared space.
         let control = new Uint8Array([42]);
         let response = Deno.core.dispatch(1, control);
         assert(response == null);
         assert(asyncRecv == 0);
         // Dispatch another message to verify that pending ops
         // are done even if shared space overflows
         Deno.core.dispatch(1, control);
         "#,
      ));
      assert_eq!(dispatch_count.load(Ordering::Relaxed), 2);
      poll_until_ready(&mut runtime, 3).unwrap();
      js_check(runtime.execute("check.js", "assert(asyncRecv == 2);"));
    });
  }

  #[test]
  fn test_pre_dispatch() {
    run_in_task(|mut cx| {
      let (mut runtime, _dispatch_count) = setup(Mode::OverflowResAsync);
      js_check(runtime.execute(
        "bad_op_id.js",
        r#"
          let thrown;
          try {
            Deno.core.dispatch(100);
          } catch (e) {
            thrown = e;
          }
          assert(String(thrown) === "TypeError: Unknown op id: 100");
         "#,
      ));
      if let Poll::Ready(Err(_)) = runtime.poll_unpin(&mut cx) {
        unreachable!();
      }
    });
  }

  #[test]
  fn core_test_js() {
    run_in_task(|mut cx| {
      let (mut runtime, _dispatch_count) = setup(Mode::Async);
      js_check(runtime.execute("core_test.js", include_str!("core_test.js")));
      if let Poll::Ready(Err(_)) = runtime.poll_unpin(&mut cx) {
        unreachable!();
      }
    });
  }

  #[test]
  fn syntax_error() {
    let mut runtime =
      JsRuntime::new(BasicState::new(), StartupData::None, false);
    let src = "hocuspocus(";
    let r = runtime.execute("i.js", src);
    let e = r.unwrap_err();
    let js_error = e.downcast::<JsError>().unwrap();
    assert_eq!(js_error.end_column, Some(11));
  }

  #[test]
  fn test_encode_decode() {
    run_in_task(|mut cx| {
      let (mut runtime, _dispatch_count) = setup(Mode::Async);
      js_check(runtime.execute(
        "encode_decode_test.js",
        include_str!("encode_decode_test.js"),
      ));
      if let Poll::Ready(Err(_)) = runtime.poll_unpin(&mut cx) {
        unreachable!();
      }
    });
  }

  #[test]
  fn will_snapshot() {
    let snapshot = {
      let mut runtime =
        JsRuntime::new(BasicState::new(), StartupData::None, true);
      js_check(runtime.execute("a.js", "a = 1 + 2"));
      runtime.snapshot()
    };

    let startup_data = StartupData::Snapshot(Snapshot::JustCreated(snapshot));
    let mut runtime2 = JsRuntime::new(BasicState::new(), startup_data, false);
    js_check(runtime2.execute("check.js", "if (a != 3) throw Error('x')"));
  }

  #[test]
  fn test_from_boxed_snapshot() {
    let snapshot = {
      let mut runtime =
        JsRuntime::new(BasicState::new(), StartupData::None, true);
      js_check(runtime.execute("a.js", "a = 1 + 2"));
      let snap: &[u8] = &*runtime.snapshot();
      Vec::from(snap).into_boxed_slice()
    };

    let startup_data = StartupData::Snapshot(Snapshot::Boxed(snapshot));
    let mut runtime2 = JsRuntime::new(BasicState::new(), startup_data, false);
    js_check(runtime2.execute("check.js", "if (a != 3) throw Error('x')"));
  }

  #[test]
  fn test_heap_limits() {
    let heap_limits = HeapLimits {
      initial: 0,
      max: 20 * 1024, // 20 kB
    };
    let mut runtime = JsRuntime::with_heap_limits(
      BasicState::new(),
      StartupData::None,
      heap_limits,
    );
    let cb_handle = runtime.thread_safe_handle();

    let callback_invoke_count = Rc::new(AtomicUsize::default());
    let inner_invoke_count = Rc::clone(&callback_invoke_count);

    runtime.add_near_heap_limit_callback(
      move |current_limit, _initial_limit| {
        inner_invoke_count.fetch_add(1, Ordering::SeqCst);
        cb_handle.terminate_execution();
        current_limit * 2
      },
    );
    let err = runtime
      .execute(
        "script name",
        r#"let s = ""; while(true) { s += "Hello"; }"#,
      )
      .expect_err("script should fail");
    assert_eq!(
      "Uncaught Error: execution terminated",
      err.downcast::<JsError>().unwrap().message
    );
    assert!(callback_invoke_count.load(Ordering::SeqCst) > 0)
  }

  #[test]
  fn test_heap_limit_cb_remove() {
    let mut runtime =
      JsRuntime::new(BasicState::new(), StartupData::None, false);

    runtime.add_near_heap_limit_callback(|current_limit, _initial_limit| {
      current_limit * 2
    });
    runtime.remove_near_heap_limit_callback(20 * 1024);
    assert!(runtime.allocations.near_heap_limit_callback_data.is_none());
  }

  #[test]
  fn test_heap_limit_cb_multiple() {
    let heap_limits = HeapLimits {
      initial: 0,
      max: 20 * 1024, // 20 kB
    };
    let mut runtime = JsRuntime::with_heap_limits(
      BasicState::new(),
      StartupData::None,
      heap_limits,
    );
    let cb_handle = runtime.thread_safe_handle();

    let callback_invoke_count_first = Rc::new(AtomicUsize::default());
    let inner_invoke_count_first = Rc::clone(&callback_invoke_count_first);
    runtime.add_near_heap_limit_callback(
      move |current_limit, _initial_limit| {
        inner_invoke_count_first.fetch_add(1, Ordering::SeqCst);
        current_limit * 2
      },
    );

    let callback_invoke_count_second = Rc::new(AtomicUsize::default());
    let inner_invoke_count_second = Rc::clone(&callback_invoke_count_second);
    runtime.add_near_heap_limit_callback(
      move |current_limit, _initial_limit| {
        inner_invoke_count_second.fetch_add(1, Ordering::SeqCst);
        cb_handle.terminate_execution();
        current_limit * 2
      },
    );

    let err = runtime
      .execute(
        "script name",
        r#"let s = ""; while(true) { s += "Hello"; }"#,
      )
      .expect_err("script should fail");
    assert_eq!(
      "Uncaught Error: execution terminated",
      err.downcast::<JsError>().unwrap().message
    );
    assert_eq!(0, callback_invoke_count_first.load(Ordering::SeqCst));
    assert!(callback_invoke_count_second.load(Ordering::SeqCst) > 0);
  }

  #[test]
  fn test_mods() {
    #[derive(Default)]
    struct ModsLoader {
      pub count: Arc<AtomicUsize>,
    }

    impl ModuleLoader for ModsLoader {
      fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _is_main: bool,
      ) -> Result<ModuleSpecifier, ErrBox> {
        self.count.fetch_add(1, Ordering::Relaxed);
        assert_eq!(specifier, "./b.js");
        assert_eq!(referrer, "file:///a.js");
        let s = ModuleSpecifier::resolve_import(specifier, referrer).unwrap();
        Ok(s)
      }

      fn load(
        &self,
        _module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dyn_import: bool,
      ) -> Pin<Box<ModuleSourceFuture>> {
        unreachable!()
      }
    }

    let loader = Rc::new(ModsLoader::default());
    let state = BasicState::new();

    let resolve_count = loader.count.clone();
    let dispatch_count = Arc::new(AtomicUsize::new(0));
    let dispatch_count_ = dispatch_count.clone();

    let dispatcher = move |_state: Rc<BasicState>, bufs: BufVec| -> Op {
      dispatch_count_.fetch_add(1, Ordering::Relaxed);
      assert_eq!(bufs.len(), 1);
      assert_eq!(bufs[0].len(), 1);
      assert_eq!(bufs[0][0], 42);
      let buf = [43u8, 0, 0, 0][..].into();
      Op::Async(futures::future::ready(buf).boxed())
    };
    state.register_op("test", dispatcher);

    let mut runtime =
      JsRuntime::new_with_loader(loader, state, StartupData::None, false);

    js_check(runtime.execute(
      "setup.js",
      r#"
        function assert(cond) {
          if (!cond) {
            throw Error("assert");
          }
        }
        "#,
    ));

    assert_eq!(dispatch_count.load(Ordering::Relaxed), 0);

    let specifier_a = "file:///a.js".to_string();
    let mod_a = runtime
      .mod_new(
        true,
        &specifier_a,
        r#"
        import { b } from './b.js'
        if (b() != 'b') throw Error();
        let control = new Uint8Array([42]);
        Deno.core.send(1, control);
      "#,
      )
      .unwrap();
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 0);

    let state_rc = JsRuntime::state(&runtime);
    {
      let state = state_rc.borrow();
      let imports = state.modules.get_children(mod_a);
      assert_eq!(
        imports,
        Some(&vec![ModuleSpecifier::resolve_url("file:///b.js").unwrap()])
      );
    }
    let mod_b = runtime
      .mod_new(false, "file:///b.js", "export function b() { return 'b' }")
      .unwrap();
    {
      let state = state_rc.borrow();
      let imports = state.modules.get_children(mod_b).unwrap();
      assert_eq!(imports.len(), 0);
    }

    js_check(runtime.mod_instantiate(mod_b));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 0);
    assert_eq!(resolve_count.load(Ordering::SeqCst), 1);

    js_check(runtime.mod_instantiate(mod_a));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 0);

    js_check(runtime.mod_evaluate(mod_a));
    assert_eq!(dispatch_count.load(Ordering::Relaxed), 1);
  }

  #[test]
  fn dyn_import_err() {
    #[derive(Clone, Default)]
    struct DynImportErrLoader {
      pub count: Arc<AtomicUsize>,
    }

    impl ModuleLoader for DynImportErrLoader {
      fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _is_main: bool,
      ) -> Result<ModuleSpecifier, ErrBox> {
        self.count.fetch_add(1, Ordering::Relaxed);
        assert_eq!(specifier, "/foo.js");
        assert_eq!(referrer, "file:///dyn_import2.js");
        let s = ModuleSpecifier::resolve_import(specifier, referrer).unwrap();
        Ok(s)
      }

      fn load(
        &self,
        _module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dyn_import: bool,
      ) -> Pin<Box<ModuleSourceFuture>> {
        async { Err(io::Error::from(io::ErrorKind::NotFound).into()) }.boxed()
      }
    }

    // Test an erroneous dynamic import where the specified module isn't found.
    run_in_task(|cx| {
      let loader = Rc::new(DynImportErrLoader::default());
      let count = loader.count.clone();
      let mut runtime = JsRuntime::new_with_loader(
        loader,
        BasicState::new(),
        StartupData::None,
        false,
      );

      js_check(runtime.execute(
        "file:///dyn_import2.js",
        r#"
        (async () => {
          await import("/foo.js");
        })();
        "#,
      ));

      assert_eq!(count.load(Ordering::Relaxed), 0);
      // We should get an error here.
      let result = runtime.poll_unpin(cx);
      if let Poll::Ready(Ok(_)) = result {
        unreachable!();
      }
      assert_eq!(count.load(Ordering::Relaxed), 2);
    })
  }

  #[derive(Clone, Default)]
  struct DynImportOkLoader {
    pub prepare_load_count: Arc<AtomicUsize>,
    pub resolve_count: Arc<AtomicUsize>,
    pub load_count: Arc<AtomicUsize>,
  }

  impl ModuleLoader for DynImportOkLoader {
    fn resolve(
      &self,
      specifier: &str,
      referrer: &str,
      _is_main: bool,
    ) -> Result<ModuleSpecifier, ErrBox> {
      let c = self.resolve_count.fetch_add(1, Ordering::Relaxed);
      assert!(c < 4);
      assert_eq!(specifier, "./b.js");
      assert_eq!(referrer, "file:///dyn_import3.js");
      let s = ModuleSpecifier::resolve_import(specifier, referrer).unwrap();
      Ok(s)
    }

    fn load(
      &self,
      specifier: &ModuleSpecifier,
      _maybe_referrer: Option<ModuleSpecifier>,
      _is_dyn_import: bool,
    ) -> Pin<Box<ModuleSourceFuture>> {
      self.load_count.fetch_add(1, Ordering::Relaxed);
      let info = ModuleSource {
        module_url_specified: specifier.to_string(),
        module_url_found: specifier.to_string(),
        code: "export function b() { return 'b' }".to_owned(),
      };
      async move { Ok(info) }.boxed()
    }

    fn prepare_load(
      &self,
      _load_id: ModuleLoadId,
      _module_specifier: &ModuleSpecifier,
      _maybe_referrer: Option<String>,
      _is_dyn_import: bool,
    ) -> Pin<Box<dyn Future<Output = Result<(), ErrBox>>>> {
      self.prepare_load_count.fetch_add(1, Ordering::Relaxed);
      async { Ok(()) }.boxed_local()
    }
  }

  #[test]
  fn dyn_import_ok() {
    run_in_task(|cx| {
      let loader = Rc::new(DynImportOkLoader::default());
      let prepare_load_count = loader.prepare_load_count.clone();
      let resolve_count = loader.resolve_count.clone();
      let load_count = loader.load_count.clone();
      let mut runtime = JsRuntime::new_with_loader(
        loader,
        BasicState::new(),
        StartupData::None,
        false,
      );

      // Dynamically import mod_b
      js_check(runtime.execute(
        "file:///dyn_import3.js",
        r#"
          (async () => {
            let mod = await import("./b.js");
            if (mod.b() !== 'b') {
              throw Error("bad1");
            }
            // And again!
            mod = await import("./b.js");
            if (mod.b() !== 'b') {
              throw Error("bad2");
            }
          })();
          "#,
      ));

      // First poll runs `prepare_load` hook.
      assert!(matches!(runtime.poll_unpin(cx), Poll::Pending));
      assert_eq!(prepare_load_count.load(Ordering::Relaxed), 1);

      // Second poll actually loads modules into the isolate.
      assert!(matches!(runtime.poll_unpin(cx), Poll::Ready(Ok(_))));
      assert_eq!(resolve_count.load(Ordering::Relaxed), 4);
      assert_eq!(load_count.load(Ordering::Relaxed), 2);
      assert!(matches!(runtime.poll_unpin(cx), Poll::Ready(Ok(_))));
      assert_eq!(resolve_count.load(Ordering::Relaxed), 4);
      assert_eq!(load_count.load(Ordering::Relaxed), 2);
    })
  }

  #[test]
  fn dyn_import_borrow_mut_error() {
    // https://github.com/denoland/deno/issues/6054
    run_in_task(|cx| {
      let loader = Rc::new(DynImportOkLoader::default());
      let prepare_load_count = loader.prepare_load_count.clone();
      let mut runtime = JsRuntime::new_with_loader(
        loader,
        BasicState::new(),
        StartupData::None,
        false,
      );
      js_check(runtime.execute(
        "file:///dyn_import3.js",
        r#"
          (async () => {
            let mod = await import("./b.js");
            if (mod.b() !== 'b') {
              throw Error("bad");
            }
            // Now do any op
            Deno.core.ops();
          })();
          "#,
      ));
      // First poll runs `prepare_load` hook.
      let _ = runtime.poll_unpin(cx);
      assert_eq!(prepare_load_count.load(Ordering::Relaxed), 1);
      // Second poll triggers error
      let _ = runtime.poll_unpin(cx);
    })
  }

  #[test]
  fn es_snapshot() {
    #[derive(Default)]
    struct ModsLoader;

    impl ModuleLoader for ModsLoader {
      fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _is_main: bool,
      ) -> Result<ModuleSpecifier, ErrBox> {
        assert_eq!(specifier, "file:///main.js");
        assert_eq!(referrer, ".");
        let s = ModuleSpecifier::resolve_import(specifier, referrer).unwrap();
        Ok(s)
      }

      fn load(
        &self,
        _module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<ModuleSpecifier>,
        _is_dyn_import: bool,
      ) -> Pin<Box<ModuleSourceFuture>> {
        unreachable!()
      }
    }

    let loader = std::rc::Rc::new(ModsLoader::default());
    let mut runtime = JsRuntime::new_with_loader(
      loader,
      BasicState::new(),
      StartupData::None,
      true,
    );

    let specifier = ModuleSpecifier::resolve_url("file:///main.js").unwrap();
    let source_code = "Deno.core.print('hello\\n')".to_string();

    let module_id = futures::executor::block_on(
      runtime.load_module(&specifier, Some(source_code)),
    )
    .unwrap();

    js_check(runtime.mod_evaluate(module_id));

    let _snapshot = runtime.snapshot();
  }
}
#![cfg(feature="game-activity")]

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::marker::PhantomData;
use std::os::raw;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use std::{thread, ptr};
use std::os::unix::prelude::*;

use log::{Level, error, trace};

use jni_sys::*;

use ndk_sys::{ALooper_wake};
use ndk_sys::{ALooper, ALooper_pollAll};

use ndk::asset::AssetManager;
use ndk::configuration::Configuration;
use ndk::looper::{FdEvent};
use ndk::native_window::NativeWindow;

use crate::{MainEvent, Rect, PollEvent, AndroidApp, NativeWindowRef};

mod ffi;

pub mod input;
use input::{MotionEvent, KeyEvent, Axis, InputEvent};


// The only time it's safe to update the android_app->savedState pointer is
// while handling a SaveState event, so this API is only exposed for those
// events...
#[derive(Debug)]
pub struct StateSaver<'a> {
    app: &'a AndroidAppInner,
}

impl<'a> StateSaver<'a> {
    pub fn store(&self, state: &'a [u8]) {

        // android_native_app_glue specifically expects savedState to have been allocated
        // via libc::malloc since it will automatically handle freeing the data once it
        // has been handed over to the Java Activity / main thread.
        unsafe {
            let app_ptr = self.app.ptr.as_ptr();

            // In case the application calls store() multiple times for some reason we
            // make sure to free any pre-existing state...
            if (*app_ptr).savedState != ptr::null_mut() {
                libc::free((*app_ptr).savedState);
                (*app_ptr).savedState = ptr::null_mut();
                (*app_ptr).savedStateSize = 0;
            }

            let buf = libc::malloc(state.len());
            if buf == ptr::null_mut() {
                panic!("Failed to allocate save_state buffer");
            }

            // Since it's a byte array there's no special alignment requirement here.
            //
            // Since we re-define `buf` we ensure it's not possible to access the buffer
            // via its original pointer for the lifetime of the slice.
            {
                let buf: &mut [u8] = std::slice::from_raw_parts_mut(buf.cast(), state.len());
                buf.copy_from_slice(state);
            }

            (*app_ptr).savedState = buf;
            (*app_ptr).savedStateSize = state.len() as u64;
        }
    }
}

#[derive(Debug)]
pub struct StateLoader<'a> {
    app: &'a AndroidAppInner,
}
impl<'a> StateLoader<'a> {
    pub fn load(&self) -> Option<Vec<u8>> {
        unsafe {
            let app_ptr = self.app.ptr.as_ptr();
            if (*app_ptr).savedState != ptr::null_mut() && (*app_ptr).savedStateSize > 0 {
                let buf: &mut [u8] = std::slice::from_raw_parts_mut((*app_ptr).savedState.cast(), (*app_ptr).savedStateSize as usize);
                let state = buf.to_vec();
                Some(state)
            } else {
                None
            }
        }
    }
}

#[derive(Clone)]
pub struct AndroidAppWaker {
    // The looper pointer is owned by the android_app and effectively
    // has a 'static lifetime, and the ALooper_wake C API is thread
    // safe, so this can be cloned safely and is send + sync safe
    looper: NonNull<ALooper>
}
unsafe impl Send for AndroidAppWaker {}
unsafe impl Sync for AndroidAppWaker {}

impl AndroidAppWaker {
    pub fn wake(&self) {
        unsafe { ALooper_wake(self.looper.as_ptr()); }
    }
}

impl AndroidApp {
    pub(crate) unsafe fn from_ptr(ptr: NonNull<ffi::android_app>) -> Self {

        // Note: we don't use from_ptr since we don't own the android_app.config
        // and need to keep in mind that the Drop handler is going to call
        // AConfiguration_delete()
        //
        // Whenever we get a ConfigChanged notification we synchronize this
        // config state with a deep copy.
        let config = Configuration::clone_from_ptr(NonNull::new_unchecked((*ptr.as_ptr()).config));

        Self {
            inner: Arc::new(AndroidAppInner {
                ptr,
                config: RwLock::new(config),
                native_window: Default::default(),
            })
        }
    }
}

#[derive(Debug)]
pub struct AndroidAppInner {
    ptr: NonNull<ffi::android_app>,
    config: RwLock<Configuration>,
    native_window: RwLock<Option<NativeWindow>>,
}

impl AndroidAppInner {

    pub fn native_window<'a>(&self) -> Option<NativeWindowRef> {
        let guard = self.native_window.read().unwrap();
        if let Some(ref window) = *guard {
            Some(NativeWindowRef::new(window))
        } else {
            None
        }
    }

    pub fn poll_events<F>(&self, timeout: Option<Duration>, mut callback: F)
        where F: FnMut(PollEvent)
    {
        trace!("poll_events");

        unsafe {
            let app_ptr = self.ptr;

            let mut fd: i32 = 0;
            let mut events: i32 = 0;
            let mut source: *mut core::ffi::c_void = ptr::null_mut();

            let timeout_milliseconds = if let Some(timeout) = timeout { timeout.as_millis() as i32 } else { -1 };
            trace!("Calling ALooper_pollAll, timeout = {timeout_milliseconds}");
            let id = ALooper_pollAll(timeout_milliseconds, &mut fd, &mut events, &mut source as *mut *mut core::ffi::c_void);
            match id {
                ffi::ALOOPER_POLL_WAKE => {
                    trace!("ALooper_pollAll returned POLL_WAKE");
                    callback(PollEvent::Wake);
                }
                ffi::ALOOPER_POLL_CALLBACK => {
                    // ALooper_pollAll is documented to handle all callback sources internally so it should
                    // never return a _CALLBACK source id...
                    error!("Spurious ALOOPER_POLL_CALLBACK from ALopper_pollAll() (ignored)");
                }
                ffi::ALOOPER_POLL_TIMEOUT => {
                    trace!("ALooper_pollAll returned POLL_TIMEOUT");
                    callback(PollEvent::Timeout);
                }
                ffi::ALOOPER_POLL_ERROR => {
                    trace!("ALooper_pollAll returned POLL_ERROR");
                    callback(PollEvent::Error);

                    // Considering that this API is quite likely to be used in `android_main`
                    // it's rather unergonomic to require the call to unwrap a Result for each
                    // call to poll_events(). Alternatively we could maybe even just panic!()
                    // here, while it's hard to imagine practically being able to recover
                    //return Err(LooperError);
                }
                id if id >= 0 => {
                    match id as u32 {
                        ffi::NativeAppGlueLooperId_LOOPER_ID_MAIN => {
                            trace!("ALooper_pollAll returned ID_MAIN");
                            let source: *mut ffi::android_poll_source = source.cast();
                            if source != ptr::null_mut() {
                                let cmd_i = ffi::android_app_read_cmd(app_ptr.as_ptr());

                                let cmd = match cmd_i as u32 {
                                    //NativeAppGlueAppCmd_UNUSED_APP_CMD_INPUT_CHANGED => AndroidAppMainEvent::InputChanged,
                                    ffi::NativeAppGlueAppCmd_APP_CMD_INIT_WINDOW => MainEvent::InitWindow {},
                                    ffi::NativeAppGlueAppCmd_APP_CMD_TERM_WINDOW => MainEvent::TerminateWindow {},
                                    ffi::NativeAppGlueAppCmd_APP_CMD_WINDOW_RESIZED => MainEvent::WindowResized {},
                                    ffi::NativeAppGlueAppCmd_APP_CMD_WINDOW_REDRAW_NEEDED => MainEvent::RedrawNeeded {},
                                    ffi::NativeAppGlueAppCmd_APP_CMD_CONTENT_RECT_CHANGED => MainEvent::ContentRectChanged,
                                    ffi::NativeAppGlueAppCmd_APP_CMD_GAINED_FOCUS => MainEvent::GainedFocus,
                                    ffi::NativeAppGlueAppCmd_APP_CMD_LOST_FOCUS => MainEvent::LostFocus,
                                    ffi::NativeAppGlueAppCmd_APP_CMD_CONFIG_CHANGED => MainEvent::ConfigChanged,
                                    ffi::NativeAppGlueAppCmd_APP_CMD_LOW_MEMORY => MainEvent::LowMemory,
                                    ffi::NativeAppGlueAppCmd_APP_CMD_START => MainEvent::Start,
                                    ffi::NativeAppGlueAppCmd_APP_CMD_RESUME => MainEvent::Resume { loader: StateLoader { app: &self } },
                                    ffi::NativeAppGlueAppCmd_APP_CMD_SAVE_STATE => MainEvent::SaveState { saver: StateSaver { app: &self } },
                                    ffi::NativeAppGlueAppCmd_APP_CMD_PAUSE => MainEvent::Pause,
                                    ffi::NativeAppGlueAppCmd_APP_CMD_STOP => MainEvent::Stop,
                                    ffi::NativeAppGlueAppCmd_APP_CMD_DESTROY => MainEvent::Destroy,
                                    ffi::NativeAppGlueAppCmd_APP_CMD_WINDOW_INSETS_CHANGED => MainEvent::InsetsChanged {},
                                    _ => unreachable!()
                                };

                                trace!("Read ID_MAIN command {cmd_i} = {cmd:?}");

                                trace!("Calling android_app_pre_exec_cmd({cmd_i})");
                                ffi::android_app_pre_exec_cmd(app_ptr.as_ptr(), cmd_i);
                                match cmd {
                                    MainEvent::ConfigChanged => {
                                        *self.config.write().unwrap() =
                                            Configuration::clone_from_ptr(NonNull::new_unchecked((*app_ptr.as_ptr()).config));
                                    }
                                    MainEvent::InitWindow { .. } => {
                                        let win_ptr = (*app_ptr.as_ptr()).window;
                                        *self.native_window.write().unwrap() =
                                            Some(NativeWindow::from_ptr(NonNull::new(win_ptr).unwrap()));
                                    }
                                    MainEvent::TerminateWindow { .. } => {
                                        *self.native_window.write().unwrap() = None;
                                    }
                                    _ => {}
                                }

                                trace!("Invoking callback for ID_MAIN command = {:?}", cmd);
                                callback(PollEvent::Main(cmd));

                                trace!("Calling android_app_post_exec_cmd({cmd_i})");
                                ffi::android_app_post_exec_cmd(app_ptr.as_ptr(), cmd_i);
                            } else {
                                panic!("ALooper_pollAll returned ID_MAIN event with NULL android_poll_source!");
                            }
                        }
                        _ => {
                            let events = FdEvent::from_bits(events as u32)
                                .expect(&format!("Spurious ALooper_pollAll event flags {:#04x}", events as u32));
                            trace!("Custom ALooper event source: id = {id}, fd = {fd}, events = {events:?}, data = {source:?}");
                            callback(PollEvent::FdEvent{ ident: id, fd: fd as RawFd, events, data: source });
                        }
                    }
                }
                _ => {
                    error!("Spurious ALooper_pollAll return value {id} (ignored)");
                }
            }
        }
    }

    pub fn enable_motion_axis(&self, axis: Axis) {
        unsafe {
            ffi::GameActivityPointerAxes_enableAxis(axis as i32)
        }
    }

    pub fn disable_motion_axis(&self, axis: Axis) {
        unsafe {
            ffi::GameActivityPointerAxes_disableAxis(axis as i32)
        }
    }

    pub fn create_waker(&self) -> AndroidAppWaker {
        unsafe {
            // From the application's pov we assume the app_ptr and looper pointer
            // have static lifetimes and we can safely assume they are never NULL.
            let app_ptr = self.ptr.as_ptr();
            AndroidAppWaker { looper: NonNull::new_unchecked((*app_ptr).looper) }
        }
    }

    pub fn config(&self) -> Configuration {
        self.config.read().unwrap().clone()
    }

    pub fn content_rect(&self) -> Rect {
        unsafe {
            let app_ptr = self.ptr.as_ptr();
            Rect {
                left: (*app_ptr).contentRect.left,
                right: (*app_ptr).contentRect.right,
                top: (*app_ptr).contentRect.top,
                bottom: (*app_ptr).contentRect.bottom,
            }
        }
    }

    pub fn asset_manager(&self) -> AssetManager {
        unsafe {
            let app_ptr = self.ptr.as_ptr();
            let am_ptr = NonNull::new_unchecked((*(*app_ptr).activity).assetManager);
            AssetManager::from_ptr(am_ptr)
        }
    }

    pub fn input_events<'b, F>(&self, mut callback: F)
        where F: FnMut(&InputEvent)
    {
        let buf = unsafe {
            let app_ptr = self.ptr.as_ptr();
            let input_buffer = ffi::android_app_swap_input_buffers(app_ptr);
            if input_buffer == ptr::null_mut() {
                return;
            }
            InputBuffer::from_ptr(NonNull::new_unchecked(input_buffer))
        };

        for key_event in buf.key_events_iter() {
            callback(&InputEvent::KeyEvent(key_event));
        }
        for motion_event in buf.motion_events_iter() {
            callback(&InputEvent::MotionEvent(motion_event));
        }
    }

    fn try_get_path_from_ptr(path: *const u8) -> Option<std::path::PathBuf> {
        if path == ptr::null() { return None; }
        let cstr = unsafe {
            let cstr_slice = CStr::from_ptr(path);
            cstr_slice.to_str().ok()?
        };
        if cstr.len() == 0 { return None; }
        Some(std::path::PathBuf::from(cstr))
    }

    pub fn internal_data_path(&self) -> Option<std::path::PathBuf> {
        unsafe {
            let app_ptr = self.ptr.as_ptr();
            Self::try_get_path_from_ptr((*(*app_ptr).activity).internalDataPath)
        }
    }

    pub fn external_data_path(&self) -> Option<std::path::PathBuf> {
        unsafe {
            let app_ptr = self.ptr.as_ptr();
            Self::try_get_path_from_ptr((*(*app_ptr).activity).externalDataPath)
        }
    }

    pub fn obb_path(&self) -> Option<std::path::PathBuf> {
        unsafe {
            let app_ptr = self.ptr.as_ptr();
            Self::try_get_path_from_ptr((*(*app_ptr).activity).obbPath)
        }
    }
}

struct MotionEventsIterator<'a> {
    pos: usize,
    count: usize,
    buffer: &'a InputBuffer<'a>
}

impl<'a> Iterator for MotionEventsIterator<'a> {
    type Item = MotionEvent;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos < self.count {
            unsafe {
                let ga_event = (*self.buffer.ptr.as_ptr()).motionEvents[self.pos];
                let event = MotionEvent::new(ga_event);
                self.pos += 1;
                Some(event)
            }
        } else {
            None
        }
    }
}

struct KeyEventsIterator<'a> {
    pos: usize,
    count: usize,
    buffer: &'a InputBuffer<'a>
}

impl<'a> Iterator for KeyEventsIterator<'a> {
    type Item = KeyEvent;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos < self.count {
            unsafe {
                let ga_event = (*self.buffer.ptr.as_ptr()).keyEvents[self.pos];
                let event = KeyEvent::new(ga_event);
                self.pos += 1;
                Some(event)
            }
        } else {
            None
        }
    }
}

struct InputBuffer<'a> {
    ptr: NonNull<ffi::android_input_buffer>,
    _lifetime: PhantomData<&'a ffi::android_input_buffer>
}

impl<'a> InputBuffer<'a> {
    pub(crate) fn from_ptr(ptr: NonNull<ffi::android_input_buffer>) -> InputBuffer<'a> {
        Self {
            ptr,
            _lifetime: PhantomData::default()
        }
    }

    // XXX: It's really not ideal here that Rust iterators can't yield values
    // that borrow from the iterator, so we implicitly have to copy the
    // events as we iterate...
    pub fn motion_events_iter<'b>(&'b self) -> MotionEventsIterator<'b> {
        unsafe {
            let count = (*self.ptr.as_ptr()).motionEventsCount as usize;
            MotionEventsIterator { pos: 0, count, buffer: self }
        }
    }

    pub fn key_events_iter<'b>(&'b self) -> KeyEventsIterator<'b> {
        unsafe {
            let count = (*self.ptr.as_ptr()).keyEventsCount as usize;
            KeyEventsIterator { pos: 0, count, buffer: self }
        }
    }
}

impl<'a> Drop for InputBuffer<'a> {
    fn drop(&mut self) {
        unsafe {
            ffi::android_app_clear_motion_events(self.ptr.as_ptr());
            ffi::android_app_clear_key_events(self.ptr.as_ptr());
        }
    }
}

// Rust doesn't give us a clean way to directly export symbols from C/C++
// so we rename the C/C++ symbols and re-export these JNI entrypoints from
// Rust...
//
// https://github.com/rust-lang/rfcs/issues/2771
extern "C" {
    pub fn Java_com_google_androidgamesdk_GameActivity_loadNativeCode_C(
        env: *mut JNIEnv,
        javaGameActivity: jobject,
        path: jstring,
        funcName: jstring,
        internalDataDir: jstring,
        obbDir: jstring,
        externalDataDir: jstring,
        jAssetMgr: jobject,
        savedState: jbyteArray,
    ) -> jlong;

    pub fn GameActivity_onCreate_C(
        activity: *mut ffi::GameActivity,
        savedState: *mut ::std::os::raw::c_void,
        savedStateSize: ffi::size_t,
    );
}

#[no_mangle]
pub unsafe extern "C" fn Java_com_google_androidgamesdk_GameActivity_loadNativeCode(
        env: *mut JNIEnv,
        java_game_activity: jobject,
        path: jstring,
        func_name: jstring,
        internal_data_dir: jstring,
        obb_dir: jstring,
        external_data_dir: jstring,
        jasset_mgr: jobject,
        saved_state: jbyteArray,
    ) -> jni_sys::jlong
{
    Java_com_google_androidgamesdk_GameActivity_loadNativeCode_C(env, java_game_activity, path, func_name,
        internal_data_dir, obb_dir, external_data_dir, jasset_mgr, saved_state)
}

#[no_mangle]
pub unsafe extern "C" fn GameActivity_onCreate(
        activity: *mut ffi::GameActivity,
        saved_state: *mut ::std::os::raw::c_void,
        saved_state_size: ffi::size_t,
    )
{
    GameActivity_onCreate_C(activity, saved_state, saved_state_size);
}

fn android_log(level: Level, tag: &CStr, msg: &CStr) {
    let prio = match level {
        Level::Error => ndk_sys::android_LogPriority_ANDROID_LOG_ERROR,
        Level::Warn => ndk_sys::android_LogPriority_ANDROID_LOG_WARN,
        Level::Info => ndk_sys::android_LogPriority_ANDROID_LOG_INFO,
        Level::Debug => ndk_sys::android_LogPriority_ANDROID_LOG_DEBUG,
        Level::Trace => ndk_sys::android_LogPriority_ANDROID_LOG_VERBOSE,
    };
    unsafe {
        ndk_sys::__android_log_write(prio as raw::c_int, tag.as_ptr(), msg.as_ptr());
    }
}

extern "Rust" {
    pub fn android_main(app: AndroidApp);
}

// This is a spring board between android_native_app_glue and the user's
// `app_main` function. This is run on a dedicated thread spawned
// by android_native_app_glue.
#[no_mangle]
pub unsafe extern "C" fn _rust_glue_entry(app: *mut ffi::android_app) {

    // Maybe make this stdout/stderr redirection an optional / opt-in feature?...
    let mut logpipe: [RawFd; 2] = Default::default();
    libc::pipe(logpipe.as_mut_ptr());
    libc::dup2(logpipe[1], libc::STDOUT_FILENO);
    libc::dup2(logpipe[1], libc::STDERR_FILENO);
    thread::spawn(move || {
        let tag = CStr::from_bytes_with_nul(b"RustStdoutStderr\0").unwrap();
        let file = File::from_raw_fd(logpipe[0]);
        let mut reader = BufReader::new(file);
        let mut buffer = String::new();
        loop {
            buffer.clear();
            if let Ok(len) = reader.read_line(&mut buffer) {
                if len == 0 {
                    break;
                } else if let Ok(msg) = CString::new(buffer.clone()) {
                    android_log(Level::Info, tag, &msg);
                }
            }
        }
    });

    let jvm: *mut JavaVM = (*(*app).activity).vm;
    let activity: jobject = (*(*app).activity).javaGameActivity;
    ndk_context::initialize_android_context(jvm.cast(), activity.cast());

    let app = AndroidApp::from_ptr(NonNull::new(app).unwrap());

    // Since this is a newly spawned thread then the JVM hasn't been attached
    // to the thread yet. Attach before calling the applications main function
    // so they can safely make JNI calls
    let mut jenv_out: *mut core::ffi::c_void = std::ptr::null_mut();
    if let Some(attach_current_thread) = (*(*jvm)).AttachCurrentThread {
        attach_current_thread(jvm, &mut jenv_out, std::ptr::null_mut());
    }

    // XXX: If we were in control of the Java Activity subclass then
    // we could potentially run the android_main function via a Java native method
    // springboard (e.g. call an Activity subclass method that calls a jni native
    // method that then just calls android_main()) that would make sure there was
    // a Java frame at the base of our call stack which would then be recognised
    // when calling FindClass to lookup a suitable classLoader, instead of
    // defaulting to the system loader. Without this then it's difficult for native
    // code to look up non-standard Java classes.
    android_main(app);

    // Since this is a newly spawned thread then the JVM hasn't been attached
    // to the thread yet. Attach before calling the applications main function
    // so they can safely make JNI calls
    if let Some(detach_current_thread) = (*(*jvm)).DetachCurrentThread {
        detach_current_thread(jvm);
    }

    ndk_context::release_android_context();
}
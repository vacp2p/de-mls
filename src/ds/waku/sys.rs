//! Raw FFI declarations matching libwaku.h (trampoline pattern).
//!
//! No `#[link]` attribute — build.rs handles linking to libwaku.dylib.
#![allow(unused)]

use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::slice;

pub type FFICallBack = unsafe extern "C" fn(c_int, *const c_char, usize, *const c_void);

unsafe extern "C" {
    pub fn waku_new(
        config_json: *const c_char,
        cb: FFICallBack,
        user_data: *const c_void,
    ) -> *mut c_void;

    pub fn waku_start(ctx: *mut c_void, cb: FFICallBack, user_data: *const c_void) -> c_int;

    pub fn waku_stop(ctx: *mut c_void, cb: FFICallBack, user_data: *const c_void) -> c_int;

    pub fn waku_version(ctx: *mut c_void, cb: FFICallBack, user_data: *const c_void) -> c_int;

    pub fn set_event_callback(ctx: *mut c_void, cb: FFICallBack, user_data: *const c_void);

    pub fn waku_relay_publish(
        ctx: *mut c_void,
        cb: FFICallBack,
        user_data: *const c_void,
        pubsub_topic: *const c_char,
        json_message: *const c_char,
        timeout_ms: c_uint,
    ) -> c_int;

    pub fn waku_relay_subscribe(
        ctx: *mut c_void,
        cb: FFICallBack,
        user_data: *const c_void,
        pubsub_topic: *const c_char,
    ) -> c_int;

    pub fn waku_connect(
        ctx: *mut c_void,
        cb: FFICallBack,
        user_data: *const c_void,
        peer_multi_addr: *const c_char,
        timeout_ms: c_uint,
    ) -> c_int;

    pub fn waku_get_my_peerid(ctx: *mut c_void, cb: FFICallBack, user_data: *const c_void)
    -> c_int;

    pub fn waku_start_discv5(ctx: *mut c_void, cb: FFICallBack, user_data: *const c_void) -> c_int;

    pub fn waku_stop_discv5(ctx: *mut c_void, cb: FFICallBack, user_data: *const c_void) -> c_int;

    pub fn waku_get_my_enr(ctx: *mut c_void, cb: FFICallBack, user_data: *const c_void) -> c_int;

    pub fn waku_discv5_update_bootnodes(
        ctx: *mut c_void,
        cb: FFICallBack,
        user_data: *const c_void,
        bootnodes: *const c_char,
    ) -> c_int;
}

// ── Trampoline pattern ──────────────────────────────────────────────────────

pub unsafe extern "C" fn trampoline<C>(
    return_val: c_int,
    buffer: *const c_char,
    buffer_len: usize,
    data: *const c_void,
) where
    C: FnMut(i32, &str),
{
    if data.is_null() {
        return;
    }
    let closure = unsafe { &mut *(data as *mut C) };
    if buffer.is_null() || buffer_len == 0 {
        closure(return_val, "");
        return;
    }
    let bytes = unsafe { slice::from_raw_parts(buffer as *const u8, buffer_len) };
    let buffer_str = String::from_utf8_lossy(bytes);
    closure(return_val, &buffer_str);
}

pub fn get_trampoline<C>(_closure: &C) -> FFICallBack
where
    C: FnMut(i32, &str),
{
    trampoline::<C>
}

pub const RET_OK: i32 = 0;

//! Safe synchronous wrapper around the raw libwaku FFI.
//!
//! `#![allow(unused)]` covers methods exposed for completeness that no
//! caller currently invokes (version, peer id, discv5 control).
#![allow(unused)]

use std::{cell::OnceCell, ffi::CString, os::raw::c_void};

use crate::ds::waku::sys::{self as waku_sys, RET_OK, get_trampoline};

/// Structured error from a libwaku FFI call. `op` carries the C symbol
/// name so `Display` output identifies which call failed.
#[derive(Debug, thiserror::Error)]
pub enum WakuFfiError {
    #[error("invalid C string for {arg}: {source}")]
    InvalidCString {
        arg: &'static str,
        #[source]
        source: std::ffi::NulError,
    },
    #[error("{op} failed (code {code}): {msg}")]
    Call {
        op: &'static str,
        code: i32,
        msg: String,
    },
    #[error("{op} returned no data")]
    NoData { op: &'static str },
}

fn invalid_cstring(arg: &'static str) -> impl FnOnce(std::ffi::NulError) -> WakuFfiError {
    move |source| WakuFfiError::InvalidCString { arg, source }
}

/// Opaque handle to a libwaku node context.
pub struct WakuNodeCtx {
    ctx: *mut c_void,
}

// Single node, all calls serialized inside C.
unsafe impl Send for WakuNodeCtx {}
unsafe impl Sync for WakuNodeCtx {}

impl WakuNodeCtx {
    /// Create a new waku node from a libwaku JSON config.
    pub fn new(config_json: &str) -> Result<Self, WakuFfiError> {
        let config_cstr = CString::new(config_json).map_err(invalid_cstring("config_json"))?;

        let mut err: Option<String> = None;
        let mut closure = |ret: i32, data: &str| {
            if ret != RET_OK {
                err = Some(data.to_string());
            }
        };
        let cb = get_trampoline(&closure);

        let ctx = unsafe {
            waku_sys::waku_new(
                config_cstr.as_ptr(),
                cb,
                &mut closure as *mut _ as *const c_void,
            )
        };

        if ctx.is_null() || err.is_some() {
            // Null ctx without a code: report 0 as a sentinel.
            return Err(WakuFfiError::Call {
                op: "waku_new",
                code: RET_OK,
                msg: err.unwrap_or_else(|| "returned null".into()),
            });
        }

        Ok(Self { ctx })
    }

    /// Start the node.
    pub fn start(&self) -> Result<(), WakuFfiError> {
        let mut err: Option<String> = None;
        let mut closure = |ret: i32, data: &str| {
            if ret != RET_OK {
                err = Some(data.to_string());
            }
        };
        let cb = get_trampoline(&closure);

        let ret =
            unsafe { waku_sys::waku_start(self.ctx, cb, &mut closure as *mut _ as *const c_void) };

        if ret != RET_OK || err.is_some() {
            return Err(WakuFfiError::Call {
                op: "waku_start",
                code: ret,
                msg: err.unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// Get the node version string.
    pub fn version(&self) -> Result<String, WakuFfiError> {
        let version: OnceCell<String> = OnceCell::new();
        let mut closure = |ret: i32, data: &str| {
            if ret == RET_OK {
                let _ = version.set(data.to_string());
            }
        };
        let cb = get_trampoline(&closure);

        let ret = unsafe {
            waku_sys::waku_version(self.ctx, cb, &mut closure as *mut _ as *const c_void)
        };

        if ret != RET_OK {
            return Err(WakuFfiError::Call {
                op: "waku_version",
                code: ret,
                msg: String::new(),
            });
        }
        version
            .into_inner()
            .ok_or(WakuFfiError::NoData { op: "waku_version" })
    }

    /// Get the local peer ID.
    pub fn get_peer_id(&self) -> Result<String, WakuFfiError> {
        let peer_id: OnceCell<String> = OnceCell::new();
        let mut closure = |ret: i32, data: &str| {
            if ret == RET_OK {
                let _ = peer_id.set(data.to_string());
            }
        };
        let cb = get_trampoline(&closure);

        let ret = unsafe {
            waku_sys::waku_get_my_peerid(self.ctx, cb, &mut closure as *mut _ as *const c_void)
        };

        if ret != RET_OK {
            return Err(WakuFfiError::Call {
                op: "waku_get_my_peerid",
                code: ret,
                msg: String::new(),
            });
        }
        peer_id.into_inner().ok_or(WakuFfiError::NoData {
            op: "waku_get_my_peerid",
        })
    }

    /// Connect to a peer by multiaddr.
    pub fn connect(&self, peer_multi_addr: &str, timeout_ms: u32) -> Result<(), WakuFfiError> {
        let addr_cstr =
            CString::new(peer_multi_addr).map_err(invalid_cstring("peer_multi_addr"))?;

        let mut err: Option<String> = None;
        let mut closure = |ret: i32, data: &str| {
            if ret != RET_OK {
                err = Some(data.to_string());
            }
        };
        let cb = get_trampoline(&closure);

        let ret = unsafe {
            waku_sys::waku_connect(
                self.ctx,
                cb,
                &mut closure as *mut _ as *const c_void,
                addr_cstr.as_ptr(),
                timeout_ms,
            )
        };

        if ret != RET_OK || err.is_some() {
            return Err(WakuFfiError::Call {
                op: "waku_connect",
                code: ret,
                msg: err.unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// Publish `json_message` (a serialized Waku relay message) on `pubsub_topic`.
    pub fn relay_publish(
        &self,
        pubsub_topic: &str,
        json_message: &str,
        timeout_ms: u32,
    ) -> Result<String, WakuFfiError> {
        let topic_cstr = CString::new(pubsub_topic).map_err(invalid_cstring("pubsub_topic"))?;
        let msg_cstr = CString::new(json_message).map_err(invalid_cstring("json_message"))?;

        let result: OnceCell<String> = OnceCell::new();
        let mut err: Option<String> = None;
        let mut closure = |ret: i32, data: &str| {
            if ret == RET_OK {
                let _ = result.set(data.to_string());
            } else {
                err = Some(data.to_string());
            }
        };
        let cb = get_trampoline(&closure);

        let ret = unsafe {
            waku_sys::waku_relay_publish(
                self.ctx,
                cb,
                &mut closure as *mut _ as *const c_void,
                topic_cstr.as_ptr(),
                msg_cstr.as_ptr(),
                timeout_ms,
            )
        };

        if ret != RET_OK || err.is_some() {
            return Err(WakuFfiError::Call {
                op: "waku_relay_publish",
                code: ret,
                msg: err.unwrap_or_default(),
            });
        }
        Ok(result.into_inner().unwrap_or_default())
    }

    /// Subscribe to a relay pubsub topic.
    pub fn relay_subscribe(&self, pubsub_topic: &str) -> Result<(), WakuFfiError> {
        let topic_cstr = CString::new(pubsub_topic).map_err(invalid_cstring("pubsub_topic"))?;

        let mut err: Option<String> = None;
        let mut closure = |ret: i32, data: &str| {
            if ret != RET_OK {
                err = Some(data.to_string());
            }
        };
        let cb = get_trampoline(&closure);

        let ret = unsafe {
            waku_sys::waku_relay_subscribe(
                self.ctx,
                cb,
                &mut closure as *mut _ as *const c_void,
                topic_cstr.as_ptr(),
            )
        };

        if ret != RET_OK || err.is_some() {
            return Err(WakuFfiError::Call {
                op: "waku_relay_subscribe",
                code: ret,
                msg: err.unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// Register the event callback. The returned `Box` must outlive the
    /// node — dropping it invalidates the FFI pointer libwaku holds.
    pub fn set_event_callback<C>(&self, closure: C) -> Box<C>
    where
        C: FnMut(i32, &str),
    {
        let mut boxed = Box::new(closure);
        let cb = get_trampoline(&*boxed);
        unsafe {
            waku_sys::set_event_callback(self.ctx, cb, &mut *boxed as *mut C as *const c_void);
        }
        boxed
    }

    /// Stop the node. Called from `Drop`, so explicit calls are usually
    /// unnecessary unless an early shutdown error needs to be surfaced.
    pub fn stop(&self) -> Result<(), WakuFfiError> {
        let mut err: Option<String> = None;
        let mut closure = |ret: i32, data: &str| {
            if ret != RET_OK {
                err = Some(data.to_string());
            }
        };
        let cb = get_trampoline(&closure);

        let ret =
            unsafe { waku_sys::waku_stop(self.ctx, cb, &mut closure as *mut _ as *const c_void) };

        if ret != RET_OK || err.is_some() {
            return Err(WakuFfiError::Call {
                op: "waku_stop",
                code: ret,
                msg: err.unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// Start the discv5 discovery service.
    pub fn start_discv5(&self) -> Result<(), WakuFfiError> {
        let mut err: Option<String> = None;
        let mut closure = |ret: i32, data: &str| {
            if ret != RET_OK {
                err = Some(data.to_string());
            }
        };
        let cb = get_trampoline(&closure);

        let ret = unsafe {
            waku_sys::waku_start_discv5(self.ctx, cb, &mut closure as *mut _ as *const c_void)
        };

        if ret != RET_OK || err.is_some() {
            return Err(WakuFfiError::Call {
                op: "waku_start_discv5",
                code: ret,
                msg: err.unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// Get the local ENR (Ethereum Node Record) string.
    pub fn get_enr(&self) -> Result<String, WakuFfiError> {
        let enr: OnceCell<String> = OnceCell::new();
        let mut closure = |ret: i32, data: &str| {
            if ret == RET_OK {
                let _ = enr.set(data.to_string());
            }
        };
        let cb = get_trampoline(&closure);

        let ret = unsafe {
            waku_sys::waku_get_my_enr(self.ctx, cb, &mut closure as *mut _ as *const c_void)
        };

        if ret != RET_OK {
            return Err(WakuFfiError::Call {
                op: "waku_get_my_enr",
                code: ret,
                msg: String::new(),
            });
        }
        enr.into_inner().ok_or(WakuFfiError::NoData {
            op: "waku_get_my_enr",
        })
    }
}

impl Drop for WakuNodeCtx {
    fn drop(&mut self) {
        if let Err(e) = self.stop() {
            tracing::warn!("waku_stop failed during drop: {e}");
        }
    }
}

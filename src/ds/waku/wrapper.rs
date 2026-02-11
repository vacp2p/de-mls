//! Safe synchronous wrapper around the raw libwaku FFI.
#![allow(unused)]
use std::cell::OnceCell;
use std::ffi::CString;
use std::os::raw::c_void;

use super::sys::{self as waku_sys, get_trampoline, RET_OK};

/// Opaque handle to a libwaku node context.
pub struct WakuNodeCtx {
    ctx: *mut c_void,
}

// The libwaku ctx pointer is thread-safe (single node, serialized calls inside C).
unsafe impl Send for WakuNodeCtx {}
unsafe impl Sync for WakuNodeCtx {}

impl WakuNodeCtx {
    /// Create a new waku node. `config_json` is the JSON string for node configuration.
    pub fn new(config_json: &str) -> Result<Self, String> {
        let config_cstr = CString::new(config_json).map_err(|e| e.to_string())?;

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
            return Err(err.unwrap_or_else(|| "waku_new returned null".into()));
        }

        Ok(Self { ctx })
    }

    /// Start the node.
    pub fn start(&self) -> Result<(), String> {
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
            return Err(err.unwrap_or_else(|| format!("waku_start returned {ret}")));
        }
        Ok(())
    }

    /// Get the node version string.
    pub fn version(&self) -> Result<String, String> {
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
            return Err(format!("waku_version returned {ret}"));
        }
        version
            .into_inner()
            .ok_or_else(|| "no version returned".into())
    }

    /// Get the local peer ID.
    pub fn get_peer_id(&self) -> Result<String, String> {
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
            return Err(format!("waku_get_my_peerid returned {ret}"));
        }
        peer_id
            .into_inner()
            .ok_or_else(|| "no peer id returned".into())
    }

    /// Connect to a peer by multiaddr.
    pub fn connect(&self, peer_multi_addr: &str, timeout_ms: u32) -> Result<(), String> {
        let addr_cstr = CString::new(peer_multi_addr).map_err(|e| e.to_string())?;

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
            return Err(err.unwrap_or_else(|| format!("waku_connect returned {ret}")));
        }
        Ok(())
    }

    /// Publish a message via relay. `json_message` is the waku message JSON.
    pub fn relay_publish(
        &self,
        pubsub_topic: &str,
        json_message: &str,
        timeout_ms: u32,
    ) -> Result<String, String> {
        let topic_cstr = CString::new(pubsub_topic).map_err(|e| e.to_string())?;
        let msg_cstr = CString::new(json_message).map_err(|e| e.to_string())?;

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
            return Err(err.unwrap_or_else(|| format!("waku_relay_publish returned {ret}")));
        }
        Ok(result.into_inner().unwrap_or_default())
    }

    /// Subscribe to a relay pubsub topic.
    pub fn relay_subscribe(&self, pubsub_topic: &str) -> Result<(), String> {
        let topic_cstr = CString::new(pubsub_topic).map_err(|e| e.to_string())?;

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
            return Err(err.unwrap_or_else(|| format!("waku_relay_subscribe returned {ret}")));
        }
        Ok(())
    }

    /// Register the event callback. Returns the boxed closure — caller must keep
    /// it alive for the lifetime of the node (dropping it invalidates the FFI pointer).
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

    /// Stop the node. Should be called before dropping to cleanly release resources.
    pub fn stop(&self) -> Result<(), String> {
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
            return Err(err.unwrap_or_else(|| format!("waku_stop returned {ret}")));
        }
        Ok(())
    }

    /// Start the discv5 discovery service.
    pub fn start_discv5(&self) -> Result<(), String> {
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
            return Err(err.unwrap_or_else(|| format!("waku_start_discv5 returned {ret}")));
        }
        Ok(())
    }

    /// Get the local ENR (Ethereum Node Record) string.
    pub fn get_enr(&self) -> Result<String, String> {
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
            return Err(format!("waku_get_my_enr returned {ret}"));
        }
        enr.into_inner().ok_or_else(|| "no ENR returned".into())
    }
}

impl Drop for WakuNodeCtx {
    fn drop(&mut self) {
        if let Err(e) = self.stop() {
            tracing::warn!("waku_stop failed during drop: {e}");
        }
    }
}

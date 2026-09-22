//! The C ABI, so this engine is not a Rust-only thing.
//!
//! Only primitives cross this boundary: pointers to NUL terminated UTF-8, integers, and one
//! function pointer. That is deliberate. A shared library whose interface is plain C can be
//! called from C, C++, Go through cgo, Python through ctypes or cffi, Ruby, Zig, and from a
//! shell script through the `dn` binary that sits on top of it, all without any of them knowing
//! Rust exists.
//!
//!   cc app.c -ldnengine        -> C and C++
//!   ctypes.CDLL("libdnengine.so")  -> Python
//!   dn get <url> -o file --json    -> shell, or any language with a subprocess call
//!
//! Every function here is safe to call from any thread, takes ownership of nothing, and never
//! unwinds into the caller: a panic is caught and turned into an error code, because unwinding
//! across a C frame is undefined behaviour.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use crate::{transport, Download};

pub const DN_OK: c_int = 0;
pub const DN_ERR_ARGS: c_int = 1;
pub const DN_ERR_NETWORK: c_int = 2;
pub const DN_ERR_INTEGRITY: c_int = 3;
pub const DN_ERR_PANIC: c_int = 4;

static LAST_ERROR: Mutex<Option<std::ffi::CString>> = Mutex::new(None);

fn set_error(msg: &str) {
    let clean = msg.replace('\0', " ");
    if let Ok(c) = std::ffi::CString::new(clean) {
        *LAST_ERROR.lock().unwrap() = Some(c);
    }
}

/// What the caller is told while it runs. Returning non-zero from this cancels the download.
pub type DnProgress = Option<extern "C" fn(done: u64, total: u64, bytes_per_second: u64,
                                           connections: c_int, user: *mut c_void) -> c_int>;

/// The knobs, laid out so a caller can zero the struct and get sensible behaviour.
#[repr(C)]
pub struct DnOptions {
    /// Connections per lane (mirror x interface). 0 means 2.
    pub streams_per_lane: c_int,
    /// Total connections. 0 means 8.
    pub max_connections: c_int,
    /// Expected sha256 as 64 hex characters, or NULL to skip the check.
    pub expect_sha256: *const c_char,
    /// Comma separated interface names, or NULL for "work it out".
    pub interfaces: *const c_char,
    /// 0 continues an interrupted download, 1 starts again.
    pub no_resume: c_int,
}

unsafe fn cstr(p: *const c_char) -> Option<String> {
    if p.is_null() { None } else { CStr::from_ptr(p).to_str().ok().map(str::to_string) }
}

/// Fetch `urls` (all serving the SAME file) into `dest`.
///
/// # Safety
/// `urls` must point to `url_count` NUL terminated strings, `dest` must be a NUL terminated
/// path, and any non-NULL field of `opts` must be NUL terminated. `user` is passed back to the
/// callback untouched and is never dereferenced here.
#[no_mangle]
pub unsafe extern "C" fn dn_download(
    urls: *const *const c_char, url_count: c_int, dest: *const c_char,
    opts: *const DnOptions, progress: DnProgress, user: *mut c_void,
) -> c_int {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if urls.is_null() || dest.is_null() || url_count <= 0 {
            set_error("dn_download needs at least one url and a destination");
            return DN_ERR_ARGS;
        }
        let mut list = Vec::new();
        for i in 0..url_count as isize {
            match cstr(*urls.offset(i)) {
                Some(u) => list.push(u),
                None => { set_error("a url was not valid UTF-8"); return DN_ERR_ARGS; }
            }
        }
        let dest = match cstr(dest) { Some(d) => d, None => { set_error("bad destination"); return DN_ERR_ARGS; } };

        let mut d = Download::new(list, &dest);
        if !opts.is_null() {
            let o = &*opts;
            if o.streams_per_lane > 0 { d = d.streams_per_lane(o.streams_per_lane as usize); }
            if o.max_connections > 0 { d = d.max_connections(o.max_connections as usize); }
            if let Some(sha) = cstr(o.expect_sha256) { d = d.expect_sha256(sha); }
            if let Some(ifs) = cstr(o.interfaces) {
                d = d.networks(ifs.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());
            }
            if o.no_resume != 0 { d = d.resume(false); }
        }

        let curl = transport::Curl::default();
        let mut cancelled = false;
        let outcome = d.run(&curl, |p| {
            if cancelled { return; }
            if let Some(cb) = progress {
                if cb(p.done, p.total, p.bytes_per_second, p.connections as c_int, user) != 0 {
                    cancelled = true;
                }
            }
        });
        match outcome {
            Ok(_) => DN_OK,
            Err(e) => {
                let integrity = e.contains("not what was published");
                set_error(&e);
                if integrity { DN_ERR_INTEGRITY } else { DN_ERR_NETWORK }
            }
        }
    }));
    match result {
        Ok(code) => code,
        Err(_) => { set_error("the engine panicked"); DN_ERR_PANIC }
    }
}

/// The last error, valid until the next call on this thread's behalf. Never NULL.
#[no_mangle]
pub extern "C" fn dn_last_error() -> *const c_char {
    static EMPTY: &[u8] = b"\0";
    match &*LAST_ERROR.lock().unwrap() {
        Some(c) => c.as_ptr(),
        None => EMPTY.as_ptr() as *const c_char,
    }
}

/// Ask what a server will allow before committing to it. Writes the size through `size_out`
/// and returns 1 when byte ranges really work (a 206, not merely an advertised header).
///
/// # Safety
/// `url` must be NUL terminated; `size_out` may be NULL.
#[no_mangle]
pub unsafe extern "C" fn dn_probe(url: *const c_char, size_out: *mut u64) -> c_int {
    let Some(u) = cstr(url) else { set_error("bad url"); return -1 };
    let p = crate::probe::probe(&u);
    if !size_out.is_null() { *size_out = p.size; }
    if p.ranges { 1 } else { 0 }
}

/// sha256 of a file, written as 64 hex characters plus a NUL into `out` (needs 65 bytes).
///
/// # Safety
/// `path` must be NUL terminated and `out` must have room for 65 bytes.
#[no_mangle]
pub unsafe extern "C" fn dn_sha256_file(path: *const c_char, out: *mut c_char) -> c_int {
    let Some(p) = cstr(path) else { set_error("bad path"); return DN_ERR_ARGS };
    if out.is_null() { set_error("no output buffer"); return DN_ERR_ARGS; }
    match crate::sha256::file(std::path::Path::new(&p)) {
        Ok(hex) => {
            std::ptr::copy_nonoverlapping(hex.as_ptr() as *const c_char, out, 64);
            *out.add(64) = 0;
            DN_OK
        }
        Err(e) => { set_error(&e.to_string()); DN_ERR_ARGS }
    }
}

/// The engine's version, as a static string.
#[no_mangle]
pub extern "C" fn dn_version() -> *const c_char {
    concat!("dnengine ", env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

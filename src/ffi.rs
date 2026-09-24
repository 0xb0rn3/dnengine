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
//! Independent calls can run on separate threads. Opaque clients reject overlapping use.
//! Inputs are borrowed; owned output buffers and handles have explicit release functions.
//! Fallible transfer calls catch Rust panics. Callers must provide valid pointers and must
//! not unwind through callbacks. See include/dnengine.h for lifetimes and thread safety.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;
use std::cell::RefCell;
use crate::http::{Client, Options, Cancellation, ItemResult, Status};

use crate::{transport, Download};

pub const DN_OK: c_int = 0;
pub const DN_ERR_ARGS: c_int = 1;
pub const DN_ERR_NETWORK: c_int = 2;
pub const DN_ERR_INTEGRITY: c_int = 3;
pub const DN_ERR_PANIC: c_int = 4;
pub const DN_ERR_CANCELLED: c_int = 5;
pub const DN_ERR_LIMIT: c_int = 6;
pub const DN_ERR_IO: c_int = 7;
pub const DN_ERR_BUSY: c_int = 8;

thread_local! {
    static LAST_ERROR: RefCell<std::ffi::CString> = RefCell::new(std::ffi::CString::default());
}
fn set_error(msg: &str) {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = std::ffi::CString::new(msg.replace('\0', " ")).unwrap());
}
fn guarded(f: impl FnOnce() -> c_int) -> c_int {
    set_error("");
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(rc) => rc,
        Err(_) => { set_error("the engine panicked"); DN_ERR_PANIC }
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
    set_error("");
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

        let curl = transport::Native::default();
        let cancel = Cancellation::new();
        let mut cancelled = false;
        let outcome = d.run_cancellable(&curl, &cancel, |p| {
            if cancelled { return; }
            if let Some(cb) = progress {
                if cb(p.done, p.total, p.bytes_per_second, p.connections as c_int, user) != 0 {
                    cancelled = true;
                    cancel.cancel();
                }
            }
        });
        if cancelled { set_error("cancelled"); return DN_ERR_CANCELLED; }
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

/// Fetch many files in one batch, reusing connections across them.
///
/// This is the entry point for the workload the range splitter cannot help: lots of small files.
/// `urls` and `dests` are parallel arrays of `count` NUL terminated strings. Returns the number
/// that succeeded, or -1 on a setup failure, with dn_last_error() holding the reason.
///
/// A single in-process pool serves this call. Use dn_fetch_many_ex with a retained client
/// for pooling across calls and owned per-item errors.
/// # Safety
/// `urls` and `dests` must contain `count` readable NUL-terminated UTF-8 strings.
/// Callback and user data must remain valid until return.
#[no_mangle]
pub unsafe extern "C" fn dn_fetch_many(
    urls: *const *const c_char, dests: *const *const c_char, count: c_int,
    parallel: c_int, progress: DnProgress, user: *mut c_void,
) -> c_int {
    // the user pointer is opaque to us and only handed back to the caller's own callback
    let user = user as usize;
    let r = std::panic::catch_unwind(move || {
        set_error("");
        if count == 0 { return 0; }
        if urls.is_null() || dests.is_null() || count < 0 { set_error("invalid batch arrays or count"); return -1; }
        let mut items = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            // SAFETY: the caller promises `count` valid pointers in each array; both are read
            // only here and nothing is retained past this call.
            let (u, d) = unsafe { (*urls.add(i), *dests.add(i)) };
            if u.is_null() || d.is_null() { set_error("null url or destination in the batch"); return -1; }
            let url = match unsafe { CStr::from_ptr(u) }.to_str() {
                Ok(v) => v.to_string(),
                Err(_) => { set_error("url is not valid utf-8"); return -1; }
            };
            let dest = match unsafe { CStr::from_ptr(d) }.to_str() {
                Ok(v) => std::path::PathBuf::from(v),
                Err(_) => { set_error("destination is not valid utf-8"); return -1; }
            };
            items.push(crate::batch::Item { url, dest });
        }
        let opts = crate::batch::BatchOpts {
            parallel: if parallel > 0 { parallel as usize } else { 8 },
            ..Default::default()
        };
        match crate::batch::fetch_many(&items, &opts, |done, total| {
            match progress {
                Some(f) => f(done as u64, total as u64, 0, 0, user as *mut c_void) == 0,
                None => true,
            }
        }) {
            Ok(r) => { if r.failed > 0 { set_error(&format!("{} batch items failed or were cancelled; use dn_fetch_many_ex for item results", r.failed)); } r.ok as c_int },
            Err(e) => { set_error(&e); -1 }
        }
    });
    match r { Ok(v) => v, Err(_) => { set_error("dn_fetch_many panicked"); -1 } }
}

#[no_mangle]
pub extern "C" fn dn_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// Ask what a server will allow before committing to it. Writes the size through `size_out`
/// and returns 1 when byte ranges really work (a 206, not merely an advertised header).
///
/// # Safety
/// `url` must be NUL terminated; `size_out` may be NULL.
#[no_mangle]
pub unsafe extern "C" fn dn_probe(url: *const c_char, size_out: *mut u64) -> c_int {
    let rc = guarded(|| {
        let Some(u) = cstr(url) else { set_error("bad url"); return -1; };
        match crate::probe::probe_cancellable(&u, &std::sync::atomic::AtomicBool::new(false)) {
            Ok(p) => {
                if !size_out.is_null() { *size_out = p.size; }
                if p.ranges { 1 } else { 0 }
            }
            Err(e) => { set_error(&e); -1 }
        }
    });
    if rc == DN_ERR_PANIC { -1 } else { rc }
}

/// sha256 of a file, written as 64 hex characters plus a NUL into `out` (needs 65 bytes).
///
/// # Safety
/// `path` must be NUL terminated and `out` must have room for 65 bytes.
#[no_mangle]
pub unsafe extern "C" fn dn_sha256_file(path: *const c_char, out: *mut c_char) -> c_int {
    guarded(|| {
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
    })
}

/// The engine's version, as a static string.
#[no_mangle]
pub extern "C" fn dn_version() -> *const c_char {
    concat!("dnengine ", env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Versioned options for the persistent HTTP client. Use dn_client_options_init.
#[repr(C)]
pub struct DnClientOptions {
    pub struct_size: u32,
    pub parallel: u32,
    pub per_host: u32,
    pub retries: u32,
    pub backoff_ms: u32,
    pub max_backoff_ms: u32,
    pub connect_timeout_ms: u32,
    pub timeout_ms: u32,
    pub total_timeout_ms: u32,
    pub ca_file: *const c_char,
}
#[repr(C)]
pub struct DnItemResult {
    pub index: u64,
    pub status: c_int,
    pub http_status: c_int,
    pub transport_code: c_int,
    pub attempts: u32,
    pub bytes: u64,
    pub error: [c_char; 256],
}
impl From<&ItemResult> for DnItemResult {
    fn from(r: &ItemResult) -> Self {
        let mut out = Self { index: r.index as u64, status: r.status as c_int, http_status: r.http_status as c_int,
            transport_code: r.transport_code, attempts: r.attempts, bytes: r.bytes, error: [0; 256] };
        let mut n = r.error.len().min(255);
        while !r.error.is_char_boundary(n) { n -= 1; }
        for (d, b) in out.error.iter_mut().zip(r.error.as_bytes()[..n].iter()) { *d = if *b == 0 { b' ' as c_char } else { *b as c_char }; }
        out
    }
}

pub struct DnClient { inner: Mutex<Client>, options: Options }
pub struct DnCancel(Cancellation);

/// # Safety
/// `out` must be NULL or point to writable storage for a complete DnClientOptions.
#[no_mangle]
pub unsafe extern "C" fn dn_client_options_init(out: *mut DnClientOptions) {
    if !out.is_null() { *out = DnClientOptions {
        struct_size: std::mem::size_of::<DnClientOptions>() as u32,
        parallel: 8, per_host: 4, retries: 2, backoff_ms: 500, max_backoff_ms: 30_000,
        connect_timeout_ms: 20_000, timeout_ms: 60_000, total_timeout_ms: 300_000, ca_file: std::ptr::null(),
    }; }
}
/// # Safety
/// `opts` must be NULL or a readable initialized DnClientOptions, with any ca_file
/// pointing to a readable NUL-terminated UTF-8 string for this call.
#[no_mangle]
pub unsafe extern "C" fn dn_client_new(opts: *const DnClientOptions) -> *mut DnClient {
    let mut out = std::ptr::null_mut();
    guarded(|| {
        let mut options = Options::default();
        if let Some(o) = opts.as_ref() {
            if o.struct_size as usize != std::mem::size_of::<DnClientOptions>() {
                set_error("wrong dn_client_options struct_size; use dn_client_options_init"); return DN_ERR_ARGS;
            }
            if o.parallel == 0 || o.parallel > 256 || o.per_host == 0 || o.per_host > 256 || o.retries > 100
                || o.backoff_ms == 0 || o.max_backoff_ms < o.backoff_ms {
                set_error("invalid client options"); return DN_ERR_ARGS;
            }
            let ms = |n: u32| std::time::Duration::from_millis(n as u64);
            options = Options { parallel: o.parallel as usize, per_host: o.per_host as usize, retries: o.retries,
                backoff: ms(o.backoff_ms), max_backoff: ms(o.max_backoff_ms), connect_timeout: ms(o.connect_timeout_ms),
                timeout: ms(o.timeout_ms), total_timeout: ms(o.total_timeout_ms), ..Options::default() };
            if !o.ca_file.is_null() {
                let Some(ca) = cstr(o.ca_file) else { set_error("invalid CA path"); return DN_ERR_ARGS; };
                options.ca_file = Some(ca);
            }
        }
        match Client::new() {
            Ok(client) => { out = Box::into_raw(Box::new(DnClient { inner: Mutex::new(client), options })); DN_OK }
            Err(e) => { set_error(&e); DN_ERR_NETWORK }
        }
    });
    out
}
/// # Safety
/// `client` must be NULL or a live pointer from dn_client_new, freed exactly once
/// after all operations and callbacks using it have returned.
#[no_mangle]
pub unsafe extern "C" fn dn_client_free(client: *mut DnClient) { if !client.is_null() { drop(Box::from_raw(client)); } }
#[no_mangle]
pub extern "C" fn dn_cancel_new() -> *mut DnCancel { Box::into_raw(Box::new(DnCancel(Cancellation::new()))) }
/// # Safety
/// `cancel` must be NULL or a live dn_cancel_new token which is not concurrently freed.
#[no_mangle]
pub unsafe extern "C" fn dn_cancel(cancel: *const DnCancel) { if let Some(c) = cancel.as_ref() { c.0.cancel(); } }
/// # Safety
/// `cancel` must be NULL or a live dn_cancel_new token, freed exactly once after
/// all transfers and cancellation calls referencing it have returned.
#[no_mangle]
pub unsafe extern "C" fn dn_cancel_free(cancel: *mut DnCancel) { if !cancel.is_null() { drop(Box::from_raw(cancel)); } }

unsafe fn items_from_c(urls: *const *const c_char, dests: *const *const c_char, count: usize) -> Result<Vec<crate::batch::Item>, String> {
    if count > c_int::MAX as usize || (count > 0 && (urls.is_null() || dests.is_null())) { return Err("invalid batch arrays or count".into()); }
    (0..count).map(|i| {
        Ok(crate::batch::Item { url: cstr(*urls.add(i)).ok_or("invalid URL string")?,
            dest: cstr(*dests.add(i)).ok_or("invalid destination string")?.into() })
    }).collect()
}
unsafe fn with_client(client: *mut DnClient, cancel: *const DnCancel,
    f: impl FnOnce(&mut Client, &Options, &Cancellation) -> c_int) -> c_int {
    let Some(c) = client.as_ref() else { set_error("null client"); return DN_ERR_ARGS; };
    let mut inner = match c.inner.try_lock() {
        Ok(g) => g,
        Err(_) => { set_error("client busy or poisoned; serialize calls or create another client"); return DN_ERR_BUSY; }
    };
    let token = cancel.as_ref().map(|c| c.0.clone()).unwrap_or_default();
    f(&mut inner, &c.options, &token)
}

/// Returns DN_OK for a completed batch operation, including per-item failures.
/// Each of count result slots is filled. A nonzero return is an operation/setup error.
/// # Safety
/// Client/token pointers must be live for the call. Input arrays contain count
/// readable strings; results points to count writable DnItemResult slots. Callbacks
/// and user data remain valid until return. No callback may unwind across C.
#[no_mangle]
pub unsafe extern "C" fn dn_fetch_many_ex(client: *mut DnClient,
    urls: *const *const c_char, dests: *const *const c_char, count: usize,
    results: *mut DnItemResult, cancel: *const DnCancel, progress: DnProgress, user: *mut c_void) -> c_int {
    guarded(|| {
        if count > 0 && results.is_null() { set_error("null results array"); return DN_ERR_ARGS; }
        let items = match items_from_c(urls, dests, count) { Ok(v) => v, Err(e) => { set_error(&e); return DN_ERR_ARGS; } };
        with_client(client, cancel, |c,o,token| {
            match c.fetch_many(&items, o, token, |done,total| progress.is_none_or(|f| f(done as u64,total as u64,0,0,user) == 0)) {
                Ok(out) => {
                    for (i, r) in out.iter().enumerate() { results.add(i).write(DnItemResult::from(r)); }
                    DN_OK
                }
                Err(e) => { set_error(&e); DN_ERR_NETWORK }
            }
        })
    })
}
/// # Safety
/// Client/token pointers and URL must be valid for the call. data and len must
/// point to writable outputs; optional result must hold one DnItemResult. Release
/// successful buffers only with dn_buffer_free and their original length.
#[no_mangle]
pub unsafe extern "C" fn dn_fetch_bytes(client: *mut DnClient, url: *const c_char, max_bytes: usize,
    data: *mut *mut u8, len: *mut usize, result: *mut DnItemResult, cancel: *const DnCancel) -> c_int {
    guarded(|| {
        if data.is_null() || len.is_null() { set_error("null buffer outputs"); return DN_ERR_ARGS; }
        *data = std::ptr::null_mut(); *len = 0;
        let Some(url) = cstr(url) else { set_error("invalid URL"); return DN_ERR_ARGS; };
        with_client(client, cancel, |c,o,token| match c.fetch_bytes(&url, max_bytes, o, token) {
            Ok(response) => {
                if !result.is_null() { result.write(DnItemResult::from(&response.result)); }
                if response.result.status != Status::Ok { set_error(&response.result.error); return response.result.status as c_int; }
                let buffer = response.body.into_boxed_slice();
                *len = buffer.len();
                *data = Box::into_raw(buffer) as *mut u8;
                DN_OK
            }
            Err(e) => { set_error(&e); DN_ERR_NETWORK }
        })
    })
}
/// # Safety
/// data must be NULL or the exact pointer returned by dn_fetch_bytes with its
/// original len, not previously freed and no longer borrowed by any reader.
#[no_mangle]
pub unsafe extern "C" fn dn_buffer_free(data: *mut u8, len: usize) {
    if !data.is_null() { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(data, len))); }
}
/// Return zero after consuming this borrowed chunk; nonzero aborts delivery.
pub type DnWrite = Option<extern "C" fn(*const u8, usize, *mut c_void) -> c_int>;
/// # Safety
/// Client/token and URL pointers must be live for the call. write/user remain
/// valid until return; result is NULL or writable. The callback must not retain
/// chunk pointers, unwind, or free its active client/token.
#[no_mangle]
pub unsafe extern "C" fn dn_fetch_stream(client: *mut DnClient, url: *const c_char, write: DnWrite,
    user: *mut c_void, result: *mut DnItemResult, cancel: *const DnCancel) -> c_int {
    guarded(|| {
        let Some(url) = cstr(url) else { set_error("invalid URL"); return DN_ERR_ARGS; };
        let Some(write) = write else { set_error("null write callback"); return DN_ERR_ARGS; };
        with_client(client, cancel, |c,o,token| {
            let mut aborted = false;
            match c.fetch_stream(&url, o, token, &mut |b| {
                if write(b.as_ptr(), b.len(), user) == 0 { Ok(()) }
                else { aborted = true; Err(std::io::Error::other("write callback aborted")) }
            }) {
                Ok(mut r) => {
                    if aborted { r.status = Status::Cancelled; }
                    if !result.is_null() { result.write(DnItemResult::from(&r)); }
                    if r.status != Status::Ok { set_error(&r.error); }
                    r.status as c_int
                }
                Err(e) => { set_error(&e); DN_ERR_NETWORK }
            }
        })
    })
}

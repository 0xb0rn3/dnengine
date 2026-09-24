//! Range transports behind one trait. Native uses system libcurl in process;
//! Curl retains the explicit subprocess option for existing Rust callers.
//! Only Native shares request caps and Retry-After policy between workers.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// An attempt that has delivered nothing for this long is abandoned so the block can be given
/// to another lane. A connection that hangs is worse than one that fails.
pub const STALL_AFTER: Duration = Duration::from_secs(20);

pub trait Transport {
    /// Probe through the same policy domain when the backend supports one.
    fn probe(&self, url: &str, stop: &AtomicBool) -> Result<crate::probe::Probe, String> {
        crate::probe::probe_cancellable(url, stop)
    }

    /// Fetch bytes `from..=to` and hand them to `sink` with the absolute offset they belong at.
    /// Returns how many bytes were delivered. `speed` is called with the running rate so the
    /// scheduler can tell a slow connection from a stopped one.
    fn range(
        &self, url: &str, net: &str, from: u64, to: u64, stop: &AtomicBool,
        sink: &mut dyn FnMut(u64, &[u8]) -> std::io::Result<()>,
        speed: &mut dyn FnMut(f64),
    ) -> Result<u64, String>;

    /// The whole file over one connection, for servers that refuse ranges.
    fn whole(
        &self, url: &str, net: &str, stop: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
    ) -> Result<u64, String>;
}

pub struct Curl {
    pub user_agent: String,
    pub connect_timeout: u32,
}

impl Default for Curl {
    fn default() -> Self {
        Curl { user_agent: format!("dnengine/{}", env!("CARGO_PKG_VERSION")), connect_timeout: 20 }
    }
}

impl Curl {
    fn spawn(&self, url: &str, net: &str, range: Option<(u64, u64)>) -> Result<std::process::Child, String> {
        let mut cmd = Command::new("curl");
        cmd.args(["-q", "-sS", "-L", "--fail", "--globoff", "--proto", "=http,https", "--proto-redir", "=https,http", "--max-time", "0", "--connect-timeout", &self.connect_timeout.to_string(),
                  "-A", &self.user_agent]);
        if let Some((a, b)) = range { cmd.args(["-r", &format!("{a}-{b}")]); }
        if !net.is_empty() { cmd.args(["--interface", net]); }
        cmd.arg("--url").arg(url).stdout(Stdio::piped()).stderr(Stdio::null());
        cmd.spawn().map_err(|e| format!("curl will not start: {e}"))
    }

    fn pump(
        &self, mut child: std::process::Child, mut at: Option<u64>, stop: &AtomicBool,
        mut on_bytes: impl FnMut(Option<u64>, &[u8]) -> std::io::Result<()>,
        speed: &mut dyn FnMut(f64), net: &str,
    ) -> Result<u64, String> {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
        }
        let mut out = child.stdout.take().ok_or("curl produced no output")?;
        let mut child = ChildGuard(child);
        use std::os::fd::AsRawFd;
        extern "C" { fn dn_curl_nonblocking(fd: std::ffi::c_int) -> std::ffi::c_int; }
        if unsafe { dn_curl_nonblocking(out.as_raw_fd()) } != 0 { return Err("cannot make curl pipe nonblocking".into()); }
        let mut buf = vec![0u8; 64 * 1024];
        let mut got = 0u64;
        let began = Instant::now();
        let mut last_byte = Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) { return Err("cancelled".into()); }
            match out.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    on_bytes(at, &buf[..n]).map_err(|e| format!("write: {e}"))?;
                    if let Some(a) = at.as_mut() { *a += n as u64; }
                    got += n as u64;
                    last_byte = Instant::now();
                    speed(got as f64 / began.elapsed().as_secs_f64().max(0.001));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(20)),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("read: {e}")),
            }
            if last_byte.elapsed() > STALL_AFTER {
                return Err(format!("a connection on {net:?} went quiet for {}s", STALL_AFTER.as_secs()));
            }
        }
        let status = child.0.wait().map_err(|e| format!("curl: {e}"))?;
        if !status.success() { return Err(format!("curl failed on {net:?}: {status}")); }
        Ok(got)
    }
}

impl Transport for Curl {
    fn range(
        &self, url: &str, net: &str, from: u64, to: u64, stop: &AtomicBool,
        sink: &mut dyn FnMut(u64, &[u8]) -> std::io::Result<()>,
        speed: &mut dyn FnMut(f64),
    ) -> Result<u64, String> {
        let child = self.spawn(url, net, Some((from, to)))?;
        self.pump(child, Some(from), stop, |at, bytes| sink(at.unwrap_or(0), bytes), speed, net)
    }

    fn whole(
        &self, url: &str, net: &str, stop: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
    ) -> Result<u64, String> {
        let child = self.spawn(url, net, None)?;
        let mut noop = |_: f64| {};
        self.pump(child, None, stop, |_, bytes| sink(bytes), &mut noop, net)
    }
}

/// In-process libcurl transport. Pools persist per calling worker; all workers share origin
/// admission and cooldowns. Keep one Native for one Download/policy domain.
pub struct Native {
    pub options: crate::http::Options,
    policy: crate::http::Policy,
    clients: std::sync::Mutex<std::collections::HashMap<std::thread::ThreadId, crate::http::Client>>,
}
impl Default for Native {
    fn default() -> Self { Self::new(crate::http::Options::default()) }
}
impl Native {
    pub fn new(options: crate::http::Options) -> Self {
        Self { options, policy: Default::default(), clients: Default::default() }
    }
    fn transfer(&self, url: &str, net: &str, range: Option<(u64,u64)>, stop: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>) -> Result<u64, String> {
        let id = std::thread::current().id();
        let cached = self.clients.lock().unwrap().remove(&id);
        let mut client = match cached { Some(c) => c, None => crate::http::Client::with_policy(self.policy.clone())? };
        let result = client.stream(url, net, range, &self.options, stop, sink);
        let mut clients = self.clients.lock().unwrap();
        if clients.len() < 64 { clients.insert(id, client); }
        drop(clients);
        let r = result?;
        if r.status == crate::http::Status::Ok { Ok(r.bytes) } else { Err(r.error) }
    }
}
impl Transport for Native {
    fn probe(&self, url: &str, stop: &AtomicBool) -> Result<crate::probe::Probe, String> {
        let mut client = crate::http::Client::with_policy(self.policy.clone())?;
        let mut opts = self.options.clone();
        opts.total_timeout = Duration::from_secs(30);
        client.probe(url, &opts, stop).map(|h| crate::probe::parse(&h))
    }

    fn range(&self, url: &str, net: &str, from: u64, to: u64, stop: &AtomicBool,
        sink: &mut dyn FnMut(u64, &[u8]) -> std::io::Result<()>, speed: &mut dyn FnMut(f64)) -> Result<u64, String> {
        let mut at = from;
        let start = Instant::now();
        self.transfer(url, net, Some((from,to)), stop, &mut |bytes| {
            sink(at, bytes)?;
            at += bytes.len() as u64;
            speed((at-from) as f64 / start.elapsed().as_secs_f64().max(0.001));
            Ok(())
        })
    }
    fn whole(&self, url: &str, net: &str, stop: &AtomicBool,
        sink: &mut dyn FnMut(&[u8]) -> std::io::Result<()>) -> Result<u64, String> {
        self.transfer(url, net, None, stop, sink)
    }
}

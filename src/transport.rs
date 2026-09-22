//! How one byte range actually gets fetched.
//!
//! This is a trait so the engine above it never learns what a socket is. Today the only
//! implementation drives curl: it is on every machine, and it brings redirects, proxies, HTTP/2
//! and a maintained TLS stack that we would otherwise have to write and keep safe ourselves.
//! When a native io_uring transport is worth having, it implements this and nothing else moves.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// An attempt that has delivered nothing for this long is abandoned so the block can be given
/// to another lane. A connection that hangs is worse than one that fails.
pub const STALL_AFTER: Duration = Duration::from_secs(20);

pub trait Transport {
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
        cmd.args(["-sS", "-L", "--max-time", "0", "--connect-timeout", &self.connect_timeout.to_string(),
                  "-A", &self.user_agent]);
        if let Some((a, b)) = range { cmd.args(["-r", &format!("{a}-{b}")]); }
        if !net.is_empty() { cmd.args(["--interface", net]); }
        cmd.arg(url).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.spawn().map_err(|e| format!("curl will not start: {e}"))
    }

    fn pump(
        &self, mut child: std::process::Child, mut at: Option<u64>, stop: &AtomicBool,
        mut on_bytes: impl FnMut(Option<u64>, &[u8]) -> std::io::Result<()>,
        speed: &mut dyn FnMut(f64), net: &str,
    ) -> Result<u64, String> {
        let mut out = child.stdout.take().ok_or("curl produced no output")?;
        let mut buf = vec![0u8; 1 << 20];
        let mut got = 0u64;
        let began = Instant::now();
        let mut last_byte = Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) { let _ = child.kill(); break; }
            match out.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    on_bytes(at, &buf[..n]).map_err(|e| format!("write: {e}"))?;
                    if let Some(a) = at.as_mut() { *a += n as u64; }
                    got += n as u64;
                    last_byte = Instant::now();
                    speed(got as f64 / began.elapsed().as_secs_f64().max(0.001));
                }
                Err(e) => { let _ = child.kill(); return Err(format!("read: {e}")); }
            }
            if last_byte.elapsed() > STALL_AFTER {
                let _ = child.kill();
                return Err(format!("a connection{} went quiet for {}s",
                    if net.is_empty() { String::new() } else { format!(" on {net}") },
                    STALL_AFTER.as_secs()));
            }
        }
        let status = child.wait().map_err(|e| format!("curl: {e}"))?;
        if !status.success() && got == 0 {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() { let _ = e.read_to_string(&mut err); }
            return Err(format!("curl failed{}: {}",
                if net.is_empty() { String::new() } else { format!(" on {net}") },
                err.trim().lines().next().unwrap_or("no detail")));
        }
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

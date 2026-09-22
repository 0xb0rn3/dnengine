//! The engine over a socket, so a tool on another machine can use it too.
//!
//! This is the third way to call dnengine, next to linking the library and running the `dn`
//! binary. It exists for the cases where the thing that wants a file is not where the bandwidth
//! is: a laptop on hotel wifi asking the box at home (with fibre, ethernet and a spare disk) to
//! pull an image, or a fleet of machines sharing one fat pipe.
//!
//! Two rules make that safe enough to ship:
//!
//!   - it binds 127.0.0.1 unless told otherwise, and a non-local bind REQUIRES a token. An open
//!     downloader on a LAN is a way to fill someone's disk and to launder traffic.
//!   - every file lands under one directory, given at startup. A request cannot choose a path,
//!     only a name, so no caller can write over /etc or somebody's keys.
//!
//! The protocol is newline delimited JSON over HTTP, because that is what every language can
//! already read.
//!
//!   POST /download  {"urls":["..."],"name":"x.iso","expect":"<sha256>","streams":4}
//!                   -> a stream of progress objects, then one done object
//!   GET  /health    -> {"event":"health","version":"..."}

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use crate::{json, transport, Download};

pub struct Server {
    pub listen: String,
    pub dir: PathBuf,
    pub token: Option<String>,
    pub max_connections: usize,
}

pub fn run(cfg: Server) -> Result<(), String> {
    let local = cfg.listen.starts_with("127.") || cfg.listen.starts_with("localhost");
    if !local && cfg.token.is_none() {
        return Err("a server reachable from the network needs --token: without one anyone who \
                    can route to this port can fill the disk and use your line".into());
    }
    std::fs::create_dir_all(&cfg.dir).map_err(|e| format!("cannot use {}: {e}", cfg.dir.display()))?;
    let listener = TcpListener::bind(&cfg.listen).map_err(|e| format!("cannot listen on {}: {e}", cfg.listen))?;
    eprintln!("dnengine serving on {} into {}", cfg.listen, cfg.dir.display());
    let cfg = std::sync::Arc::new(cfg);
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let cfg = std::sync::Arc::clone(&cfg);
                std::thread::spawn(move || { let _ = handle(s, &cfg); });
            }
            Err(e) => eprintln!("connection failed: {e}"),
        }
    }
    Ok(())
}

fn reply(s: &mut TcpStream, code: &str, body: &str) -> std::io::Result<()> {
    write!(s, "HTTP/1.1 {code}\r\nContent-Type: application/x-ndjson\r\n\
               Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}")
}

fn handle(mut stream: TcpStream, cfg: &Server) -> std::io::Result<()> {
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let mut parts = request.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));

    let mut length = 0usize;
    let mut token = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 { break; }
        let trimmed = line.trim_end();
        if trimmed.is_empty() { break; }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") { length = v.trim().parse().unwrap_or(0); }
        if let Some(v) = trimmed.to_ascii_lowercase().strip_prefix("authorization: bearer ") {
            token = Some(v.trim().to_string());
        }
    }

    if path == "/health" {
        return reply(&mut stream, "200 OK",
            &format!("{}\n", json::obj(&[("event", json::s("health")),
                                         ("version", json::s(env!("CARGO_PKG_VERSION")))])));
    }
    if let Some(want) = &cfg.token {
        if token.as_deref() != Some(want.to_ascii_lowercase().as_str()) {
            eprintln!("refused {peer}: bad token");
            return reply(&mut stream, "401 Unauthorized",
                &format!("{}\n", json::obj(&[("event", json::s("error")),
                                             ("message", json::s("bad or missing token"))])));
        }
    }
    if method != "POST" || path != "/download" {
        return reply(&mut stream, "404 Not Found",
            &format!("{}\n", json::obj(&[("event", json::s("error")),
                                         ("message", json::s("POST /download or GET /health"))])));
    }

    let mut body = vec![0u8; length.min(64 * 1024)];
    std::io::Read::read_exact(&mut reader, &mut body)?;
    let body = String::from_utf8_lossy(&body).into_owned();

    let urls = strings_field(&body, "urls");
    let name = string_field(&body, "name").unwrap_or_else(|| {
        urls.first().map(|u| u.split('?').next().unwrap_or(u).rsplit('/').next().unwrap_or("download").to_string())
            .unwrap_or_else(|| "download".into())
    });
    let expect = string_field(&body, "expect");
    let streams = number_field(&body, "streams").unwrap_or(4) as usize;

    // a name, never a path: the caller does not get to choose where on this machine it lands
    let safe = name.rsplit('/').next().unwrap_or("download").replace("..", "");
    if safe.is_empty() || urls.is_empty() {
        return reply(&mut stream, "400 Bad Request",
            &format!("{}\n", json::obj(&[("event", json::s("error")),
                                         ("message", json::s("need urls, and a usable name"))])));
    }
    let dest: PathBuf = Path::new(&cfg.dir).join(&safe);

    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\
                    Cache-Control: no-store\r\nConnection: close\r\n\r\n")?;
    stream.flush()?;
    eprintln!("{peer} asked for {} -> {}", urls[0], dest.display());

    let mut d = Download::new(urls, dest.clone()).max_connections(cfg.max_connections)
        .streams_per_lane(streams);
    if let Some(e) = &expect { d = d.expect_sha256(e.clone()); }

    let mut out = stream.try_clone()?;
    let mut last = std::time::Instant::now();
    let result = d.run(&transport::Curl::default(), |p| {
        if last.elapsed() < std::time::Duration::from_millis(400) && p.done < p.total { return; }
        last = std::time::Instant::now();
        let line = json::obj(&[("event", json::s("progress")), ("done", json::V::N(p.done)),
                               ("total", json::V::N(p.total)),
                               ("bytes_per_second", json::V::N(p.bytes_per_second)),
                               ("connections", json::V::N(p.connections as u64))]);
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    });

    let line = match result {
        Ok(o) => json::obj(&[("event", json::s("done")), ("name", json::s(&safe)),
                             ("size", json::V::N(o.size)), ("sha256", json::s(&o.sha256)),
                             ("seconds", json::V::N(o.seconds as u64))]),
        Err(e) => json::obj(&[("event", json::s("error")), ("message", json::s(e))]),
    };
    writeln!(stream, "{line}")?;
    stream.flush()
}

/// Small hand parsers, because the whole request shape is three fields and a list. Anything
/// unrecognised is simply absent, which is the safe direction for every field here.
fn string_field(body: &str, key: &str) -> Option<String> {
    let at = body.find(&format!("\"{key}\""))?;
    let rest = &body[at + key.len() + 2..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    if !after.starts_with('"') { return None; }
    let mut out = String::new();
    let mut chars = after[1..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => { if let Some(n) = chars.next() { out.push(n); } }
            c => out.push(c),
        }
    }
    None
}

fn number_field(body: &str, key: &str) -> Option<u64> {
    let at = body.find(&format!("\"{key}\""))?;
    let rest = &body[at + key.len() + 2..];
    let colon = rest.find(':')?;
    let digits: String = rest[colon + 1..].trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn strings_field(body: &str, key: &str) -> Vec<String> {
    let Some(at) = body.find(&format!("\"{key}\"")) else { return vec![] };
    let rest = &body[at..];
    let Some(open) = rest.find('[') else { return vec![] };
    let Some(close) = rest[open..].find(']') else { return vec![] };
    let mut out = Vec::new();
    let mut chars = rest[open + 1..open + close].chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' { continue; }
        let mut s = String::new();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => { if let Some(n) = chars.next() { s.push(n); } }
                c => s.push(c),
            }
        }
        if !s.is_empty() { out.push(s); }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_is_read_without_a_json_library() {
        let body = r#"{"urls":["https://a/x.iso","https://b/x.iso"],"name":"x.iso","streams":6}"#;
        assert_eq!(strings_field(body, "urls"),
                   vec!["https://a/x.iso".to_string(), "https://b/x.iso".to_string()]);
        assert_eq!(string_field(body, "name").as_deref(), Some("x.iso"));
        assert_eq!(number_field(body, "streams"), Some(6));
        assert_eq!(string_field(body, "expect"), None);
    }

    #[test]
    fn a_caller_cannot_choose_a_path_only_a_name() {
        // what handle() does with the name, checked directly: no traversal survives it
        for attempt in ["../../etc/passwd", "/etc/shadow", "..//..//x"] {
            let safe = attempt.rsplit('/').next().unwrap_or("download").replace("..", "");
            assert!(!safe.contains('/') && !safe.contains(".."), "{attempt} escaped as {safe}");
        }
    }
}

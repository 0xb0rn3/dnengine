//! dn: the command line over the engine, for tools and for people.
//!
//!   dn get <url>... -o <file> [--expect <sha256>] [--streams N] [--json]
//!   dn probe <url>              what the server will allow
//!   dn net                      the ways onto the internet this machine has
//!
//! Several urls for the SAME file are a feature: they are raced and combined.

use dnengine::{commas, human, json, netif, probe, serve, sha256, state, transport, Download, Progress};
use std::path::PathBuf;
use std::process::exit;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GRN: &str = "\x1b[32m";
const YEL: &str = "\x1b[33m";
const RST: &str = "\x1b[0m";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let jsonl = args.iter().any(|a| a == "--json");
    match args.get(1).map(String::as_str).unwrap_or("help") {
        "get" | "fetch" => get(&args[2..], jsonl),
        "probe" => {
            let url = args.get(2).cloned().unwrap_or_else(|| die("usage: dn probe <url>", false));
            let p = probe::probe(&url);
            if jsonl {
                json::line(&[("event", json::s("probe")), ("size", json::V::N(p.size)),
                             ("ranges", json::V::B(p.ranges)),
                             ("filename", match p.filename { Some(f) => json::s(f), None => json::V::Null })]);
            } else {
                println!("  size   {} ({} bytes)", human(p.size), commas(p.size));
                println!("  ranges {}", if p.ranges { "yes, it can be fetched in parallel" }
                                        else { "no, one connection only" });
                if let Some(f) = p.filename { println!("  name   {f}"); }
            }
        }
        "serve" => {
            let listen = opt(&args, &["--listen", "-l"]).unwrap_or_else(|| "127.0.0.1:7878".into());
            let dir = PathBuf::from(opt(&args, &["--dir", "-d"]).unwrap_or_else(|| ".".into()));
            let token = opt(&args, &["--token"]);
            let max = opt(&args, &["--max"]).and_then(|v| v.parse().ok()).unwrap_or(8usize);
            if let Err(e) = serve::run(serve::Server { listen, dir, token, max_connections: max }) {
                die(&e, jsonl);
            }
        }
        "net" | "networks" => {
            let n = netif::usable();
            if jsonl { json::line(&[("event", json::s("networks")), ("networks", json::V::Strs(n))]); }
            else if n.is_empty() { eprintln!("  {RED}!!{RST} no interface is up"); exit(1); }
            else {
                println!("{BOLD}  downloads can use{RST}");
                for i in &n { println!("    {i}"); }
            }
        }
        "--version" | "-V" => println!("dnengine {} ({} sha256)", env!("CARGO_PKG_VERSION"), sha256::engine()),
        _ => usage(),
    }
}

fn usage() {
    println!("{BOLD}dnengine{RST} {} - many connections, every network, resumable, verified",
        env!("CARGO_PKG_VERSION"));
    println!();
    println!("  dn get <url>... -o <file>      fetch a file, using every lane it can");
    println!("  dn probe <url>                 what the server will allow");
    println!("  dn net                         the ways onto the internet this machine has");
    println!("  dn serve --dir <d> [--token t] lend the engine to other machines");
    println!();
    println!("{BOLD}get options{RST}");
    println!("  -o, --out <file>    where it lands (default: the name in the url)");
    println!("  --expect <sha256>   refuse the result unless it hashes to this");
    println!("  --streams <n>       connections per lane (default 2)");
    println!("  --max <n>           total connections (default 8)");
    println!("  --iface <a,b>       use only these interfaces");
    println!("  --single            one connection, no ranges");
    println!("  --no-resume         start again rather than continuing");
    println!("  --json              one json object per line, for tools");
    println!();
    println!("{DIM}  Give several urls for the same file and they are used together: a mirror that");
    println!("  throttles is routed around, and a slow block is raced by a second connection.{RST}");
}

fn die(msg: &str, jsonl: bool) -> ! {
    if jsonl { json::line(&[("event", json::s("error")), ("message", json::s(msg))]); }
    else { eprintln!("  {RED}!!{RST} {msg}"); }
    exit(1)
}

fn opt(args: &[String], names: &[&str]) -> Option<String> {
    args.iter().position(|a| names.contains(&a.as_str()))
        .and_then(|i| args.get(i + 1)).cloned()
}

fn bar(p: &Progress) {
    const SEGS: usize = 24;
    let filled = if p.total > 0 { (p.done as u128 * SEGS as u128 / p.total as u128) as usize } else { 0 };
    let b: String = (0..SEGS).map(|i| if i < filled { '\u{25B0}' } else { '\u{25B1}' }).collect();
    let pct = if p.total > 0 { p.done * 100 / p.total } else { 0 };
    let eta = if p.bytes_per_second > 0 { (p.total - p.done) / p.bytes_per_second } else { 0 };
    print!("\r  {YEL}{b}{RST} {pct:>3}%  {:>13} / {:<13} {:>9}/s  {} conn  eta {:02}:{:02}  ",
        commas(p.done), commas(p.total), human(p.bytes_per_second), p.connections, eta / 60, eta % 60);
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn get(args: &[String], jsonl: bool) {
    let urls: Vec<String> = args.iter().take_while(|a| !a.starts_with('-'))
        .filter(|a| a.contains("://")).cloned().collect();
    if urls.is_empty() { die("usage: dn get <url>... -o <file>", jsonl); }

    let dest = PathBuf::from(opt(args, &["-o", "--out"]).unwrap_or_else(|| {
        urls[0].split('?').next().unwrap_or("download")
            .rsplit('/').next().unwrap_or("download").to_string()
    }));

    let streams = opt(args, &["--streams"]).and_then(|v| v.parse().ok()).unwrap_or(2usize);
    let max = opt(args, &["--max"]).and_then(|v| v.parse().ok()).unwrap_or(8usize);
    let ifaces: Vec<String> = opt(args, &["--iface"]).map(|v|
        v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();

    let curl = transport::Curl::default();

    if args.iter().any(|a| a == "--single") {
        if !jsonl { println!("{BOLD}>>{RST} one connection, as asked"); }
        match dnengine::single(&curl, &urls[0], &dest) {
            Ok(()) => finish(&dest, jsonl, None),
            Err(e) => die(&e, jsonl),
        }
        return;
    }

    let mut d = Download::new(urls.clone(), dest.clone())
        .streams_per_lane(streams)
        .max_connections(max)
        .resume(!args.iter().any(|a| a == "--no-resume"));
    if !ifaces.is_empty() { d = d.networks(ifaces); }
    if let Some(sha) = opt(args, &["--expect"]) { d = d.expect_sha256(sha); }

    if !jsonl {
        let nets = netif::usable();
        println!("{BOLD}>>{RST} {} mirror{}, {} over {}", urls.len(),
            if urls.len() == 1 { "" } else { "s" },
            if nets.len() > 1 { "several networks" } else { "one network" },
            if nets.is_empty() { "the default route".into() } else { nets.join(", ") });
    }

    let mut last_emit = std::time::Instant::now();
    let result = d.run(&curl, |p| {
        if jsonl {
            if last_emit.elapsed() < std::time::Duration::from_millis(200) && p.done < p.total { return; }
            last_emit = std::time::Instant::now();
            json::line(&[("event", json::s("progress")), ("done", json::V::N(p.done)),
                         ("total", json::V::N(p.total)), ("bytes_per_second", json::V::N(p.bytes_per_second)),
                         ("connections", json::V::N(p.connections as u64)),
                         ("duplicated", json::V::N(p.duplicated))]);
        } else { bar(p); }
    });

    match result {
        Ok(o) => {
            if !jsonl { println!(); }
            let rate = (o.size as f64 / o.seconds.max(0.001)) as u64;
            if jsonl {
                json::line(&[("event", json::s("done")), ("path", json::s(o.path.to_string_lossy())),
                             ("size", json::V::N(o.size)), ("sha256", json::s(&o.sha256)),
                             ("seconds", json::V::N(o.seconds as u64)),
                             ("bytes_per_second", json::V::N(rate)),
                             ("connections", json::V::N(o.connections as u64)),
                             ("mirrors", json::V::N(o.mirrors as u64)),
                             ("duplicated", json::V::N(o.duplicated))]);
            } else {
                println!("  {GRN}ok{RST} {} in {:.0}s ({}/s over {} connections)",
                    human(o.size), o.seconds, human(rate), o.connections);
                println!("  {DIM}sha256 {}{RST}", o.sha256);
                if o.duplicated > 0 {
                    println!("  {DIM}{} fetched twice, racing a slow connection{RST}", human(o.duplicated));
                }
            }
        }
        Err(e) => {
            if !jsonl { println!(); }
            // a partial file plus its record is not rubbish: it is where the next run starts
            if state::path(&dest).exists() && !jsonl {
                eprintln!("  {DIM}what arrived so far is kept; run the same command to continue{RST}");
            }
            die(&e, jsonl);
        }
    }
}

fn finish(dest: &std::path::Path, jsonl: bool, expect: Option<&str>) {
    match sha256::file(dest) {
        Ok(sha) => {
            if let Some(w) = expect {
                if !w.eq_ignore_ascii_case(&sha) { die("what arrived is not what was published", jsonl); }
            }
            let size = dest.metadata().map(|m| m.len()).unwrap_or(0);
            if jsonl {
                json::line(&[("event", json::s("done")), ("path", json::s(dest.to_string_lossy())),
                             ("size", json::V::N(size)), ("sha256", json::s(&sha))]);
            } else {
                println!("  {GRN}ok{RST} {} {}", human(size), dest.display());
                println!("  {DIM}sha256 {sha}{RST}");
            }
        }
        Err(e) => die(&format!("cannot read back {}: {e}", dest.display()), jsonl),
    }
}

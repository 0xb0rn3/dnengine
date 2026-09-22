//! Using the engine from Rust.
//!
//!     cargo run -- https://host/file.iso /tmp/file.iso [sha256]

use dnengine::{human, transport, Download};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <url> <dest> [sha256]", args[0]);
        std::process::exit(2);
    }

    // Several urls for the same file would be raced and combined; one is the common case.
    let mut job = Download::new(vec![args[1].clone()], &args[2]).streams_per_lane(4);
    if let Some(sha) = args.get(3) { job = job.expect_sha256(sha.clone()); }

    let result = job.run(&transport::Curl::default(), |p| {
        if p.total > 0 {
            print!("\r  {:>3}%  {} / {}  {}/s  {} connections ",
                p.done * 100 / p.total, human(p.done), human(p.total),
                human(p.bytes_per_second), p.connections);
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    });

    println!();
    match result {
        Ok(o) => println!("  ok {} in {:.0}s\n  sha256 {}", human(o.size), o.seconds, o.sha256),
        Err(e) => { eprintln!("  failed: {e}"); std::process::exit(1); }
    }
}

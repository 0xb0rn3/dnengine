//! A persistent client for both batch files and bounded memory delivery.
use dnengine::{batch::Item, http::{Cancellation, Client, Options, Status}};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args.len() % 2 != 0 {
        return Err("usage: rust-app URL DEST [URL DEST ...]".into());
    }
    let items: Vec<_> = args.chunks(2).map(|p| Item { url: p[0].clone(), dest: p[1].clone().into() }).collect();
    let mut client = Client::new()?;
    let options = Options { per_host: 2, ..Default::default() };
    let cancel = Cancellation::new(); // clone this into another thread to cancel
    let results = client.fetch_many(&items, &options, &cancel, |_,_| true)?;
    for r in &results { println!("{}: {:?} HTTP={} bytes={} {}", r.index, r.status, r.http_status, r.bytes, r.error); }
    if results.iter().any(|r| r.status != Status::Ok) { return Err("batch contained failures".into()); }
    let response = client.fetch_bytes(&items[0].url, 16 * 1024 * 1024, &options, &cancel)?;
    if response.result.status != Status::Ok { return Err(response.result.error.into()); }
    println!("memory: {} bytes", response.body.len());
    Ok(())
}

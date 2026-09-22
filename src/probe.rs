//! Asking a server what it has, before committing to it.
//!
//! A one byte ranged GET rather than HEAD: it costs the same and, unlike HEAD, a 206 answer
//! proves ranges actually work rather than merely being advertised. Some mirrors advertise
//! Accept-Ranges and then ignore the header.

use std::process::Command;

pub struct Probe {
    pub size: u64,
    pub ranges: bool,
    pub filename: Option<String>,
}

pub fn probe(url: &str) -> Probe {
    let out = Command::new("curl")
        .args(["-sS", "-L", "-D", "-", "-o", "/dev/null", "--max-time", "30", "-r", "0-0",
               "-A", concat!("dnengine/", env!("CARGO_PKG_VERSION")), url])
        .output();
    let head = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    parse(&head)
}

pub fn parse(head: &str) -> Probe {
    let mut size = 0u64;
    let mut ranges = false;
    let mut filename = None;
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("content-range:") {
            // Content-Range: bytes 0-0/4223172608
            if let Some(total) = line.rsplit('/').next() {
                if let Ok(n) = total.trim().parse::<u64>() { size = n; ranges = true; }
            }
        } else if lower.starts_with("content-length:") && size == 0 {
            if let Some(v) = line.split(':').nth(1) { size = v.trim().parse().unwrap_or(0); }
        } else if lower.starts_with("content-disposition:") {
            if let Some(rest) = lower.split("filename=").nth(1) {
                let name = rest.trim().trim_matches(|c| c == '"' || c == '\'' || c == ';');
                if !name.is_empty() { filename = Some(name.to_string()); }
            }
        }
    }
    Probe { size, ranges, filename }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_206_with_a_content_range_is_what_proves_ranges_work() {
        let head = "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/4223172608\r\nContent-Length: 1\r\n";
        let p = parse(head);
        assert!(p.ranges);
        assert_eq!(p.size, 4_223_172_608);
    }

    #[test]
    fn a_plain_200_gives_a_size_but_no_ranges() {
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 370147328\r\nAccept-Ranges: bytes\r\n";
        let p = parse(head);
        assert_eq!(p.size, 370_147_328);
        assert!(!p.ranges, "the header alone is a claim, not proof");
    }

    #[test]
    fn a_filename_comes_out_of_content_disposition() {
        let head = "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/10\r\n\
                    Content-Disposition: attachment; filename=\"win11.iso\"\r\n";
        assert_eq!(parse(head).filename.as_deref(), Some("win11.iso"));
    }
}

//! What already landed, so an interrupted download continues instead of starting again.
//!
//! The record sits beside the file and is only trusted when it describes this exact url and
//! size: a mirror that has since published a new build must not be stitched into the old one.

use std::path::{Path, PathBuf};

pub fn path(dest: &Path) -> PathBuf {
    let mut p = dest.to_path_buf();
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    p.set_file_name(format!(".{name}.dnengine"));
    p
}

pub fn load(dest: &Path, url: &str, size: u64) -> Vec<usize> {
    let body = match std::fs::read_to_string(path(dest)) { Ok(b) => b, Err(_) => return vec![] };
    parse(&body, url, size)
}

pub fn parse(body: &str, url: &str, size: u64) -> Vec<usize> {
    let mut lines = body.lines();
    if lines.next() != Some(url) { return vec![]; }
    if lines.next().and_then(|l| l.parse::<u64>().ok()) != Some(size) { return vec![]; }
    lines.next().unwrap_or("").split(',').filter_map(|x| x.trim().parse().ok()).collect()
}

pub fn save(dest: &Path, url: &str, size: u64, done: &[usize]) {
    let list: Vec<String> = done.iter().map(|i| i.to_string()).collect();
    let _ = std::fs::write(path(dest), format!("{url}\n{size}\n{}\n", list.join(",")));
}

pub fn clear(dest: &Path) { let _ = std::fs::remove_file(path(dest)); }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_for_another_file_is_ignored_rather_than_stitched_in() {
        let body = "https://a/x.iso\n100\n0,1,2\n";
        assert_eq!(parse(body, "https://a/x.iso", 100), vec![0, 1, 2]);
        assert!(parse(body, "https://b/x.iso", 100).is_empty(), "different url");
        assert!(parse(body, "https://a/x.iso", 101).is_empty(), "the build changed size");
    }

    #[test]
    fn a_missing_or_damaged_record_just_means_start_over() {
        assert!(parse("", "u", 1).is_empty());
        assert!(parse("u\nnot-a-number\n1,2\n", "u", 1).is_empty());
    }
}

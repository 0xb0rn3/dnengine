//! Which ways onto the internet this machine has.
//!
//! Every one of them can carry its own connections, and that is where the speeds add up: wifi
//! plus ethernet plus a tethered phone downloads the same file about as fast as the three of
//! them together, rather than as fast as whichever one the routing table happened to pick.

/// Interfaces with a route, that are up, and that are not loopback or a virtual bridge going
/// nowhere. Reads /proc and /sys rather than shelling out, so it works in an installer too.
pub fn usable() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(routes) = std::fs::read_to_string("/proc/net/route") {
        for line in routes.lines().skip(1) {
            if let Some(iface) = line.split_whitespace().next() {
                if iface != "lo" && !out.iter().any(|x| x == iface) { out.push(iface.to_string()); }
            }
        }
    }
    out.retain(|i| !is_virtual(i) && is_up(i));
    out
}

fn is_virtual(name: &str) -> bool {
    name.starts_with("virbr") || name.starts_with("docker") || name.starts_with("br-")
        || name.starts_with("veth") || name.starts_with("tun") || name.starts_with("tap")
}

fn is_up(name: &str) -> bool {
    std::fs::read_to_string(format!("/sys/class/net/{name}/operstate"))
        .map(|s| { let s = s.trim(); s == "up" || s == "unknown" })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_interfaces_are_not_a_way_onto_the_internet() {
        for name in ["virbr0", "docker0", "br-1a2b", "veth9f2", "tun0"] {
            assert!(is_virtual(name), "{name} should be skipped");
        }
        for name in ["wlan0", "eth0", "enp0s31f6", "usb0"] {
            assert!(!is_virtual(name), "{name} is a real interface");
        }
    }
}

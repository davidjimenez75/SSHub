use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

/// Sentinel stored in [`PingResult`] sample history when `ping` times out or
/// the host is otherwise unreachable. Never a real RTT.
pub const PING_UNREACHABLE: u32 = u32::MAX;

pub fn is_unreachable(ms: u32) -> bool {
    ms == PING_UNREACHABLE
}

/// How to bucket a host for header stats / list dots from its ping history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingClass {
    /// No probe yet (or empty history).
    Unknown,
    Unreachable,
    Online,
    Slow,
}

pub fn classify_ping(samples: Option<&[u32]>) -> PingClass {
    match samples.and_then(|s| s.last().copied()) {
        None => PingClass::Unknown,
        Some(ms) if is_unreachable(ms) => PingClass::Unreachable,
        Some(ms) if ms < 100 => PingClass::Online,
        Some(_) => PingClass::Slow,
    }
}

#[derive(Debug, Clone)]
pub struct PingResult {
    pub host_name: String,
    pub address: String,
    pub latency_ms: Option<u32>, // None = timeout/unreachable
}

/// Spawn a background thread that pings the given addresses every `interval`.
/// Returns a Receiver that yields PingResult as they come in.
pub fn spawn_ping_worker(hosts: Vec<(String, String)>, interval: Duration) -> Receiver<PingResult> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        ping_loop(hosts, interval, tx);
    });
    rx
}

fn ping_loop(hosts: Vec<(String, String)>, interval: Duration, tx: Sender<PingResult>) {
    loop {
        for (name, address) in &hosts {
            let result = ping_once(name, address);
            if tx.send(result).is_err() {
                return; // Receiver dropped, exit thread
            }
        }
        thread::sleep(interval);
    }
}

fn ping_once(name: &str, address: &str) -> PingResult {
    // Unix: `ping -c 1 -W 1` (1 attempt, 1 second timeout)
    // Windows: `ping -n 1 -w 1000` (count + timeout in milliseconds)
    #[cfg(windows)]
    let output = Command::new("ping")
        .args(["-n", "1", "-w", "1000", address])
        .output();
    #[cfg(not(windows))]
    let output = Command::new("ping")
        .args(["-c", "1", "-W", "1", address])
        .output();

    let latency_ms = match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_ping_time(&stdout)
        }
        _ => None,
    };

    PingResult {
        host_name: name.to_string(),
        address: address.to_string(),
        latency_ms,
    }
}

fn parse_ping_time(output: &str) -> Option<u32> {
    // Linux/macOS: "time=12.3 ms"
    // Windows EN: "time=12ms" / "time<1ms"
    // Windows ES: "Tiempo=12ms" / "Media = 12ms"
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        for key in ["time=", "tiempo=", "time<", "tiempo<"] {
            if let Some(pos) = lower.find(key) {
                let after = &line[pos + key.len()..];
                let num_str: String = after
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                if num_str.is_empty() {
                    // time<1ms → treat as 1ms
                    return Some(1);
                }
                if let Ok(ms) = num_str.parse::<f64>() {
                    return Some(ms.round() as u32);
                }
            }
        }
        // Fallback: "Average = 12ms" / "Media = 12ms"
        for key in ["average = ", "media = "] {
            if let Some(pos) = lower.find(key) {
                let after = &line[pos + key.len()..];
                let num_str: String = after
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                if let Ok(ms) = num_str.parse::<f64>() {
                    return Some(ms.round() as u32);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ping_buckets() {
        use super::{classify_ping, PingClass, PING_UNREACHABLE};
        assert_eq!(classify_ping(None), PingClass::Unknown);
        assert_eq!(classify_ping(Some(&[])), PingClass::Unknown);
        assert_eq!(classify_ping(Some(&[50])), PingClass::Online);
        assert_eq!(classify_ping(Some(&[120])), PingClass::Slow);
        assert_eq!(
            classify_ping(Some(&[PING_UNREACHABLE])),
            PingClass::Unreachable
        );
    }

    #[test]
    fn parse_linux_ping() {
        let out = "64 bytes from 10.0.0.1: icmp_seq=1 ttl=64 time=12.3 ms";
        assert_eq!(parse_ping_time(out), Some(12));
    }

    #[test]
    fn parse_macos_ping() {
        let out = "64 bytes from 10.0.0.1: icmp_seq=0 ttl=64 time=0.847 ms";
        assert_eq!(parse_ping_time(out), Some(1));
    }

    #[test]
    fn parse_no_match() {
        assert_eq!(parse_ping_time("Request timeout"), None);
    }

    #[test]
    fn parse_multiline_output() {
        let out = "PING 10.0.0.1 (10.0.0.1) 56(84) bytes of data.\n\
                   64 bytes from 10.0.0.1: icmp_seq=1 ttl=64 time=3.45 ms\n\
                   \n\
                   --- 10.0.0.1 ping statistics ---\n\
                   1 packets transmitted, 1 received, 0% packet loss, time 0ms\n\
                   rtt min/avg/max/mdev = 3.450/3.450/3.450/0.000 ms";
        assert_eq!(parse_ping_time(out), Some(3));
    }
}

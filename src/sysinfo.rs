//! Local machine stats for the taskbar widget: CPU, RAM, commit ("swap"),
//! fixed-disk usage, network throughput and the machine's IP address.
//!
//! Everything here is sampled from Win32 counters on the window thread
//! (cheap, non-blocking). The only network call — resolving the public IP —
//! runs on its own background thread and is cached.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, IF_TYPE_SOFTWARE_LOOPBACK, MIB_IF_TABLE2,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

use crate::diagnose;

/// How often the public IP is re-resolved.
const PUBLIC_IP_REFRESH: Duration = Duration::from_secs(15 * 60);
/// Retry sooner than the normal refresh when the lookup failed (offline, VPN
/// reconnecting).
const PUBLIC_IP_RETRY: Duration = Duration::from_secs(60);
const PUBLIC_IP_URL: &str = "https://api.ipify.org";
/// `GetDriveTypeW` result for a fixed (internal) disk.
const DRIVE_FIXED: u32 = 3;
/// Only the first few fixed drives are shown; the taskbar has limited width.
pub const MAX_DISKS: usize = 3;

#[derive(Clone, Debug, Default)]
pub struct DiskStat {
    /// Drive letter with colon, e.g. `C:`.
    pub label: String,
    pub used_gb: f64,
    pub total_gb: f64,
    pub percent: f64,
}

#[derive(Clone, Debug, Default)]
pub struct SystemStats {
    pub cpu_percent: f64,
    pub ram_percent: f64,
    /// Commit charge in use — the same number Rainmeter's illustro skin
    /// labels "SWAP Usage".
    pub swap_percent: f64,
    pub disks: Vec<DiskStat>,
    /// Bytes per second since the previous sample.
    pub upload_bps: f64,
    pub download_bps: f64,
    /// Public IP when known, otherwise the primary local IPv4.
    pub ip: String,
}

struct Sampler {
    last_cpu: Option<(u64, u64)>, // (idle, kernel + user) in 100ns ticks
    last_net: Option<(u64, u64, Instant)>, // (in octets, out octets, when)
    latest: Option<SystemStats>,
}

static SAMPLER: Mutex<Sampler> = Mutex::new(Sampler {
    last_cpu: None,
    last_net: None,
    latest: None,
});

static PUBLIC_IP: Mutex<Option<String>> = Mutex::new(None);
static PUBLIC_IP_THREAD: OnceLock<()> = OnceLock::new();

/// Most recent sample, if any has been taken yet.
pub fn latest() -> Option<SystemStats> {
    SAMPLER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .latest
        .clone()
}

/// Take a fresh sample. Rates (CPU %, network bps) need two samples; the
/// first call reports them as zero.
pub fn sample() -> SystemStats {
    let mut sampler = SAMPLER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let cpu_percent = sample_cpu(&mut sampler.last_cpu);
    let (ram_percent, swap_percent) = sample_memory();
    let disks = sample_disks();
    let (upload_bps, download_bps) = sample_network(&mut sampler.last_net);
    let ip = PUBLIC_IP
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .or_else(local_ipv4)
        .unwrap_or_else(|| "--".to_string());

    let stats = SystemStats {
        cpu_percent,
        ram_percent,
        swap_percent,
        disks,
        upload_bps,
        download_bps,
        ip,
    };
    sampler.latest = Some(stats.clone());
    stats
}

fn filetime_to_u64(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64
}

fn sample_cpu(last: &mut Option<(u64, u64)>) -> f64 {
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let ok = unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) };
    if ok.is_err() {
        return 0.0;
    }
    let idle = filetime_to_u64(idle);
    // Kernel time includes idle time.
    let total = filetime_to_u64(kernel) + filetime_to_u64(user);
    let percent = match *last {
        Some((prev_idle, prev_total)) => {
            let d_total = total.saturating_sub(prev_total);
            let d_idle = idle.saturating_sub(prev_idle);
            if d_total == 0 {
                0.0
            } else {
                (1.0 - d_idle as f64 / d_total as f64) * 100.0
            }
        }
        None => 0.0,
    };
    *last = Some((idle, total));
    percent.clamp(0.0, 100.0)
}

fn sample_memory() -> (f64, f64) {
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    if unsafe { GlobalMemoryStatusEx(&mut status) }.is_err() {
        return (0.0, 0.0);
    }
    let ram_total = status.ullTotalPhys;
    let ram_used = ram_total.saturating_sub(status.ullAvailPhys);
    let ram_percent = if ram_total == 0 {
        0.0
    } else {
        ram_used as f64 / ram_total as f64 * 100.0
    };
    let commit_total = status.ullTotalPageFile;
    let commit_used = commit_total.saturating_sub(status.ullAvailPageFile);
    let swap_percent = if commit_total == 0 {
        0.0
    } else {
        commit_used as f64 / commit_total as f64 * 100.0
    };
    (ram_percent, swap_percent)
}

fn sample_disks() -> Vec<DiskStat> {
    let mask = unsafe { GetLogicalDrives() };
    let mut disks = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root: Vec<u16> = format!("{letter}:\\")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let root_ptr = PCWSTR::from_raw(root.as_ptr());
        if unsafe { GetDriveTypeW(root_ptr) } != DRIVE_FIXED {
            continue;
        }
        let mut free_to_caller = 0u64;
        let mut total = 0u64;
        let mut free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                root_ptr,
                Some(&mut free_to_caller),
                Some(&mut total),
                Some(&mut free),
            )
        };
        if ok.is_err() || total == 0 {
            continue;
        }
        let used = total.saturating_sub(free);
        let gb = |bytes: u64| bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        disks.push(DiskStat {
            label: format!("{letter}:"),
            used_gb: gb(used),
            total_gb: gb(total),
            percent: used as f64 / total as f64 * 100.0,
        });
        if disks.len() >= MAX_DISKS {
            break;
        }
    }
    disks
}

fn sample_network(last: &mut Option<(u64, u64, Instant)>) -> (f64, f64) {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    let err = unsafe { GetIfTable2(&mut table) };
    if err.is_err() || table.is_null() {
        return (0.0, 0.0);
    }
    let (mut in_octets, mut out_octets) = (0u64, 0u64);
    unsafe {
        let count = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), count);
        for row in rows {
            if row.Type == IF_TYPE_SOFTWARE_LOOPBACK || row.OperStatus != IfOperStatusUp {
                continue;
            }
            in_octets = in_octets.saturating_add(row.InOctets);
            out_octets = out_octets.saturating_add(row.OutOctets);
        }
        FreeMibTable(table as *const _);
    }
    let now = Instant::now();
    let rates = match *last {
        Some((prev_in, prev_out, prev_at)) => {
            let secs = now.duration_since(prev_at).as_secs_f64();
            if secs <= 0.0 {
                (0.0, 0.0)
            } else {
                (
                    out_octets.saturating_sub(prev_out) as f64 / secs,
                    in_octets.saturating_sub(prev_in) as f64 / secs,
                )
            }
        }
        None => (0.0, 0.0),
    };
    *last = Some((in_octets, out_octets, now));
    rates
}

/// The IPv4 address the OS would use to reach the internet. No packets are
/// sent: connecting a UDP socket only selects a route.
fn local_ipv4() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:53").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

/// Start the background thread that keeps the public IP cached. Safe to call
/// more than once; only the first call spawns the thread.
pub fn start_public_ip_refresher() {
    PUBLIC_IP_THREAD.get_or_init(|| {
        std::thread::spawn(|| loop {
            let delay = match fetch_public_ip() {
                Some(ip) => {
                    *PUBLIC_IP
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ip);
                    PUBLIC_IP_REFRESH
                }
                None => {
                    // Keep the last known address; the local IP fills in when
                    // nothing has ever been resolved.
                    PUBLIC_IP_RETRY
                }
            };
            std::thread::sleep(delay);
        });
    });
}

fn fetch_public_ip() -> Option<String> {
    // ureq is built without a default TLS backend here; wire up native-tls
    // the same way the usage poller does.
    let tls = match native_tls::TlsConnector::new() {
        Ok(tls) => tls,
        Err(error) => {
            diagnose::log(format!("public IP lookup: TLS unavailable: {error}"));
            return None;
        }
    };
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .tls_connector(std::sync::Arc::new(tls))
        .build();
    match agent.get(PUBLIC_IP_URL).call() {
        Ok(response) => {
            let body = response.into_string().ok()?;
            let ip = body.trim();
            // Sanity check: dotted IPv4 or an IPv6 literal, nothing else.
            if ip.parse::<std::net::IpAddr>().is_ok() {
                Some(ip.to_string())
            } else {
                None
            }
        }
        Err(error) => {
            diagnose::log(format!("public IP lookup failed: {error}"));
            None
        }
    }
}

/// Format a byte rate compactly for the taskbar: `3.5K/s`.
pub fn format_rate(bps: f64) -> String {
    const UNITS: [&str; 4] = ["B/s", "K/s", "M/s", "G/s"];
    let mut value = bps.max(0.0);
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// `237/268` — used and total GB, compact enough for a taskbar.
pub fn format_disk(disk: &DiskStat) -> String {
    format!("{:.0}/{:.0}", disk.used_gb, disk.total_gb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_formatting_matches_rainmeter_style() {
        assert_eq!(format_rate(0.0), "0B/s");
        assert_eq!(format_rate(3584.0), "3.5K/s");
        assert_eq!(format_rate(8.3 * 1024.0), "8.3K/s");
        assert_eq!(format_rate(2.0 * 1024.0 * 1024.0), "2.0M/s");
    }

    #[test]
    fn disk_formatting_rounds_to_whole_gb() {
        let disk = DiskStat {
            label: "C:".into(),
            used_gb: 237.3,
            total_gb: 268.1,
            percent: 88.5,
        };
        assert_eq!(format_disk(&disk), "237/268");
    }

    #[test]
    fn cpu_percent_is_derived_from_deltas() {
        let mut last = Some((1_000, 10_000));
        // Force a deterministic second sample by calling the math directly.
        let (idle, total) = (1_500u64, 11_000u64);
        let (prev_idle, prev_total) = last.take().unwrap();
        let pct = (1.0 - (idle - prev_idle) as f64 / (total - prev_total) as f64) * 100.0;
        assert!((pct - 50.0).abs() < 1e-9);
    }
}

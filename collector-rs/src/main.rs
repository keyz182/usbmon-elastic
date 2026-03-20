//! usbmon-collector — captures USB bandwidth metrics via libpcap/usbmon
//! and emits NDJSON for Elastic Agent ingestion.
//!
//! Replaces the usbtop-based Python collector.  Runs as a long-lived
//! systemd daemon (Type=simple); no timer unit required.
//!
//! ## How it works
//!
//! The Linux kernel's usbmon subsystem exposes USB traffic as pcap-
//! compatible capture interfaces named `usbmon0`, `usbmon1`, etc.
//! Each packet corresponds to one USB Request Block (URB) and carries a
//! fixed-layout header described in Documentation/usb/usbmon.txt.
//!
//! We open every available `usbmonN` interface with libpcap, run one
//! capture thread per interface, accumulate per-device byte counts over
//! a configurable interval, then write a snapshot as NDJSON.
//!
//! ## Configuration (environment variables)
//!
//! | Variable              | Default                                    | Notes                              |
//! |-----------------------|--------------------------------------------|------------------------------------|
//! | USBMON_OUTPUT_FILE    | /var/log/usbtop-metrics/usbtop.ndjson      | Also accepts USBTOP_OUTPUT_FILE    |
//! | USBMON_INTERVAL_SEC   | 60                                         | Reporting / flush interval         |
//! | USBMON_LOG_LEVEL      | INFO                                       | DEBUG enables per-packet logging   |

use pcap::{Capture, Device};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Graceful shutdown flag — set to true by the signal handler
// ---------------------------------------------------------------------------

static SHOULD_STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    SHOULD_STOP.store(true, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

struct Config {
    output_file:   PathBuf,
    interval_secs: u64,
    verbose:       bool,
}

impl Config {
    fn from_env() -> Self {
        // Accept both the new USBMON_ prefix and the old USBTOP_ prefix so
        // existing service-file Environment= lines keep working unchanged.
        let output_file = std::env::var("USBMON_OUTPUT_FILE")
            .or_else(|_| std::env::var("USBTOP_OUTPUT_FILE"))
            .unwrap_or_else(|_| "/var/log/usbtop-metrics/usbtop.ndjson".into());

        let interval_secs: u64 = std::env::var("USBMON_INTERVAL_SEC")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);

        let verbose = std::env::var("USBMON_LOG_LEVEL")
            .or_else(|_| std::env::var("USBTOP_LOG_LEVEL"))
            .map(|v| v.to_uppercase() == "DEBUG")
            .unwrap_or(false);

        Self {
            output_file: PathBuf::from(output_file),
            interval_secs,
            verbose,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-device byte counters — accumulated within each reporting interval
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct DeviceStats {
    /// Bytes device → host  (URB Complete callbacks, type == b'C')
    in_bytes: u64,
    /// Bytes host → device  (URB Submit, type == b'S')
    out_bytes: u64,
}

// Key is (bus_number, device_address).
type StatsMap = Arc<Mutex<HashMap<(u16, u8), DeviceStats>>>;

// ---------------------------------------------------------------------------
// sysfs device-name lookup
//
// /sys/bus/usb/devices/ contains one entry per enumerated device.
// Each entry has `busnum`, `devnum`, and optionally `product`.
// ---------------------------------------------------------------------------

fn lookup_device_name(bus: u16, device: u8) -> String {
    let base = Path::new("/sys/bus/usb/devices");
    let entries = match fs::read_dir(base) {
        Ok(e)  => e,
        Err(_) => return String::new(),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let busnum: Option<u16> = fs::read_to_string(path.join("busnum"))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        let devnum: Option<u8> = fs::read_to_string(path.join("devnum"))
            .ok()
            .and_then(|s| s.trim().parse().ok());

        if busnum == Some(bus) && devnum == Some(device) {
            // Prefer the human-readable product string.
            if let Ok(name) = fs::read_to_string(path.join("product")) {
                let name = name.trim().to_string();
                if !name.is_empty() {
                    return name;
                }
            }
            // Fall back to idVendor:idProduct hex codes.
            let v = fs::read_to_string(path.join("idVendor"))
                .unwrap_or_default()
                .trim()
                .to_string();
            let p = fs::read_to_string(path.join("idProduct"))
                .unwrap_or_default()
                .trim()
                .to_string();
            if !v.is_empty() {
                return format!("{}:{}", v, p);
            }
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Helpers: ISO-8601 timestamp without the chrono crate
// ---------------------------------------------------------------------------

fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let (y, mo, d) = epoch_days_to_ymd(secs / 86400);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo, d, h, m, s)
}

/// Days-since-Unix-epoch → (year, month, day) using Howard Hinnant's algorithm.
/// https://howardhinnant.github.io/date_algorithms.html#civil_from_days
fn epoch_days_to_ymd(z: u64) -> (u64, u64, u64) {
    let z   = z + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y   = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp  = (5 * doy + 2) / 153;
    let d   = doy - (153 * mp + 2) / 5 + 1;
    let mo  = if mp < 10 { mp + 3 } else { mp - 9 };
    let y   = if mo <= 2 { y + 1 } else { y };
    (y, mo, d)
}

// ---------------------------------------------------------------------------
// Helpers: minimal JSON string escaping (sufficient for sysfs product names)
// ---------------------------------------------------------------------------

fn json_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"'  => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            '\n' => vec!['\\', 'n'],
            '\r' => vec!['\\', 'r'],
            '\t' => vec!['\\', 't'],
            c    => vec![c],
        })
        .collect()
}

fn hostname() -> String {
    fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

// ---------------------------------------------------------------------------
// NDJSON flush
//
// The output schema is intentionally identical to the Python collector so
// existing Elastic Agent configurations and index mappings need no changes.
// ---------------------------------------------------------------------------

fn flush_ndjson(
    output_file:   &Path,
    stats:         &HashMap<(u16, u8), DeviceStats>,
    interval_secs: u64,
    host:          &str,
) {
    let ts         = now_iso8601();
    let interval_f = interval_secs as f64;

    let file = match OpenOptions::new().create(true).append(true).open(output_file) {
        Ok(f)  => f,
        Err(e) => {
            eprintln!("[ERROR] Cannot open output file {}: {}", output_file.display(), e);
            return;
        }
    };
    let mut w = BufWriter::new(file);

    if stats.is_empty() {
        // Heartbeat — signals to Elastic that the collector is alive.
        let _ = writeln!(
            w,
            r#"{{"@timestamp":"{ts}","host":{{"name":"{host}"}},"usbtop":{{"no_activity":true}},"event":{{"dataset":"usbtop.metrics","module":"usbtop"}}}}"#
        );
        return;
    }

    for (&(bus, device), s) in stats {
        let in_kbps  = s.in_bytes  as f64 / (interval_f * 1_024.0);
        let out_kbps = s.out_bytes as f64 / (interval_f * 1_024.0);
        let name     = json_escape(&lookup_device_name(bus, device));
        let _ = writeln!(
            w,
            r#"{{"@timestamp":"{ts}","host":{{"name":"{host}"}},"usbtop":{{"bus":{bus},"device":{device},"device_name":"{name}","in_kbps":{in_kbps:.3},"out_kbps":{out_kbps:.3}}},"event":{{"dataset":"usbtop.metrics","module":"usbtop"}}}}"#
        );
    }
}

// ---------------------------------------------------------------------------
// pcap capture loop — one thread per usbmon interface
//
// Linux USB pcap header layout (DLT_USB_LINUX / DLT_USB_LINUX_MMAPPED):
//
//   offset  0–7   id        u64   URB identifier
//   offset  8     type      u8    'S'=Submit (host→device), 'C'=Complete, 'E'=Error
//   offset  9     xfer_type u8    0=iso, 1=intr, 2=ctrl, 3=bulk
//   offset 10     epnum     u8    endpoint number (bit 7 = direction)
//   offset 11     devnum    u8    USB device address
//   offset 12–13  busnum    u16le USB bus number
//
// packet.header.len is the full on-wire packet length including header + any
// captured payload.  We use it as a conservative proxy for transfer size,
// exactly as usbtop does.
// ---------------------------------------------------------------------------

fn capture_loop(device_name: String, stats: StatsMap, verbose: bool) {
    eprintln!("[INFO]  Opening pcap on {}", device_name);

    let mut cap = match Capture::from_device(device_name.as_str())
        .expect("pcap from_device")
        // We only need the USB header (48 bytes for DLT_USB_LINUX, 64 for
        // DLT_USB_LINUX_MMAPPED); 96 bytes gives headroom for both variants.
        .snaplen(96)
        // Read timeout in ms — allows checking SHOULD_STOP between bursts.
        .timeout(500)
        .open()
    {
        Ok(c)  => c,
        Err(e) => {
            eprintln!("[ERROR] pcap open {}: {}", device_name, e);
            return;
        }
    };

    while !SHOULD_STOP.load(Ordering::Relaxed) {
        match cap.next_packet() {
            Ok(packet) => {
                let bytes = packet.data;
                // Need at least 14 bytes to read bus + device fields.
                if bytes.len() < 14 {
                    continue;
                }

                let urb_type = bytes[8];
                let devnum   = bytes[11];
                let busnum   = u16::from_le_bytes([bytes[12], bytes[13]]);
                let pkt_len  = u64::from(packet.header.len);

                let mut map = stats.lock().unwrap();
                let entry = map.entry((busnum, devnum)).or_default();

                // b'C' = Complete = data arriving from the device (in).
                // All other types (Submit, Error) count as outbound traffic.
                if urb_type == b'C' {
                    entry.in_bytes  += pkt_len;
                } else {
                    entry.out_bytes += pkt_len;
                }

                if verbose {
                    eprintln!(
                        "[DEBUG] {} type={} bus={} dev={} len={}",
                        device_name,
                        urb_type as char,
                        busnum,
                        devnum,
                        pkt_len,
                    );
                }
            }

            Err(pcap::Error::TimeoutExpired) => {
                // Normal — libpcap returned after the read timeout.
                // Loop back around and check SHOULD_STOP.
            }

            Err(e) => {
                eprintln!("[ERROR] pcap {} : {}", device_name, e);
                break;
            }
        }
    }

    eprintln!("[INFO]  Capture thread exiting for {}", device_name);
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let cfg = Config::from_env();

    // Register signal handlers so SIGTERM / SIGINT set SHOULD_STOP gracefully.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as libc::sighandler_t);
        libc::signal(libc::SIGINT,  on_signal as libc::sighandler_t);
    }

    eprintln!("[INFO]  usbmon-collector starting");
    eprintln!("[INFO]  output={} interval={}s", cfg.output_file.display(), cfg.interval_secs);

    // Ensure the output directory exists.
    if let Some(parent) = cfg.output_file.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            eprintln!("[WARN]  mkdir {}: {}", parent.display(), e);
        }
    }

    // Discover usbmon capture interfaces.
    let all_devices = match Device::list() {
        Ok(d)  => d,
        Err(e) => {
            eprintln!("[ERROR] pcap device list failed: {}", e);
            std::process::exit(1);
        }
    };

    let usbmon_devices: Vec<_> = all_devices
        .into_iter()
        .filter(|d| d.name.starts_with("usbmon"))
        .collect();

    if usbmon_devices.is_empty() {
        eprintln!("[ERROR] No usbmon pcap devices found.");
        eprintln!("[ERROR] Is usbmon loaded?  Try: modprobe usbmon");
        std::process::exit(1);
    }

    for d in &usbmon_devices {
        eprintln!("[INFO]  Monitoring {}", d.name);
    }

    // Shared stats map — written by capture threads, drained by main thread.
    let stats: StatsMap = Arc::new(Mutex::new(HashMap::new()));

    // Spawn one capture thread per usbmon interface.
    let handles: Vec<_> = usbmon_devices
        .into_iter()
        .map(|dev| {
            let stats   = Arc::clone(&stats);
            let verbose = cfg.verbose;
            thread::spawn(move || capture_loop(dev.name, stats, verbose))
        })
        .collect();

    let host = hostname();

    // Main flush loop — sleep for interval_secs (in 500 ms slices so we can
    // respond to SIGTERM promptly), then snapshot + clear counters, then write.
    while !SHOULD_STOP.load(Ordering::Relaxed) {
        let deadline = Instant::now() + Duration::from_secs(cfg.interval_secs);
        while Instant::now() < deadline && !SHOULD_STOP.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(500));
        }

        // Snapshot and reset in one lock acquisition.
        let snapshot = {
            let mut map = stats.lock().unwrap();
            let snap = map.clone();
            map.clear();
            snap
        };

        eprintln!("[INFO]  Flushing {} device record(s)", snapshot.len());
        flush_ndjson(&cfg.output_file, &snapshot, cfg.interval_secs, &host);
    }

    // Allow capture threads to observe SHOULD_STOP and exit cleanly.
    eprintln!("[INFO]  Shutting down — waiting for capture threads");
    for handle in handles {
        let _ = handle.join();
    }
    eprintln!("[INFO]  usbmon-collector stopped");
}

//! Small shared helpers: stable hashing, time formatting, atomic file writes.

use anyhow::{Context, Result};
use std::path::Path;

/// FNV-1a 64-bit. Used for the config fingerprint; we implement it ourselves
/// because `DefaultHasher` has no cross-version stability guarantee and the
/// fingerprint is persisted in state files.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Local-time ISO-8601 with milliseconds and numeric offset, e.g.
/// `2026-09-10T09:35:00.123+0800`.
pub fn iso8601_local(ms_since_epoch: i64) -> String {
    let secs = ms_since_epoch.div_euclid(1000);
    let millis = ms_since_epoch.rem_euclid(1000);
    let t = secs as nix::libc::time_t;
    let mut tm: nix::libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r only writes into `tm`, which is a valid local.
    unsafe { nix::libc::localtime_r(&t, &mut tm) };
    let stamp = strftime(&tm, c"%Y-%m-%dT%H:%M:%S");
    let zone = strftime(&tm, c"%z");
    format!("{stamp}.{millis:03}{zone}")
}

fn strftime(tm: &nix::libc::tm, fmt: &std::ffi::CStr) -> String {
    let mut buf = [0i8; 64];
    // SAFETY: `tm` and `fmt` are valid; `buf` is a correctly sized local buffer.
    let written = unsafe {
        nix::libc::strftime(
            buf.as_mut_ptr(),
            buf.len(),
            fmt.as_ptr(),
            tm as *const nix::libc::tm,
        )
    };
    if written == 0 {
        return String::new();
    }
    let bytes: Vec<u8> = buf[..written].iter().map(|b| *b as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Human readable duration such as `2m3s`, `1h02m`, `850ms`.
pub fn format_duration_ms(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let total_secs = ms / 1000;
    let (h, m, s) = (total_secs / 3600, (total_secs % 3600) / 60, total_secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Write `bytes` to `path` atomically via a temp file in `tmp_dir` + rename.
pub fn write_atomic(path: &Path, tmp_dir: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(tmp_dir)
        .with_context(|| format!("cannot create {}", tmp_dir.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmp".to_string());
    let tmp = tmp_dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot publish {}", path.display()))?;
    Ok(())
}

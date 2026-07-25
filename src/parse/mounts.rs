//! Parse and classify `/proc/mounts` lines (fstab-style escaping).

/// Filesystem types treated as real data storage for the Health tab.
pub const DATA_FSTYPES: &[&str] = &[
    "ext4", "ext3", "ext2", "btrfs", "xfs", "f2fs", "vfat", "msdos", "ntfs", "ntfs3", "exfat",
    "zfs",
];

/// One decoded mounts(5) line (source, target, fstype).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    pub source: String,
    pub target: String,
    pub fstype: String,
}

/// Unescape fstab octal sequences used by mounts(5)/fstab(5).
///
/// Only the four documented forms are decoded: `\040` space, `\011` tab,
/// `\012` newline, `\134` backslash. All other bytes (including UTF-8 multi-byte
/// sequences and malformed escapes) are preserved unchanged so non-ASCII
/// mountpoints stay valid paths for `statvfs`.
pub fn fstab_unescape(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let b1 = bytes[i + 1];
            let b2 = bytes[i + 2];
            let b3 = bytes[i + 3];
            // Strict octal digits 0–7 only (not ASCII '8'/'9').
            if is_octal_digit(b1) && is_octal_digit(b2) && is_octal_digit(b3) {
                let v = (b1 - b'0') * 64 + (b2 - b'0') * 8 + (b3 - b'0');
                // Only emit the four standard fstab escapes; leave others literal.
                if matches!(v, b' ' | b'\t' | b'\n' | b'\\') {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Input was valid UTF-8; we only rewrite ASCII escape sequences to ASCII.
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

fn is_octal_digit(b: u8) -> bool {
    (b'0'..=b'7').contains(&b)
}

/// Parse a single `/proc/mounts` line into source/target/fstype.
pub fn parse_mounts_line(line: &str) -> Option<MountEntry> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut parts = line.split_whitespace();
    let source = fstab_unescape(parts.next()?);
    let target = fstab_unescape(parts.next()?);
    let fstype = parts.next()?.to_string();
    Some(MountEntry {
        source,
        target,
        fstype,
    })
}

/// True if this mount should appear under Storage.
///
/// Rule: allowlisted data fstype + source begins with `/dev/` (block-backed).
pub fn is_health_mount(entry: &MountEntry) -> bool {
    if !DATA_FSTYPES.iter().any(|t| *t == entry.fstype) {
        return false;
    }
    entry.source.starts_with("/dev/")
}

/// Parse full mounts table text; return filtered entries (unsorted).
pub fn parse_health_mounts(content: &str) -> Vec<MountEntry> {
    let mut out = Vec::new();
    for line in content.lines() {
        if let Some(e) = parse_mounts_line(line)
            && is_health_mount(&e)
        {
            out.push(e);
        }
    }
    out
}

/// Capacity from `statvfs` field values (all sizes in filesystem units).
///
/// - `total = blocks * frsize`
/// - `available = bavail * frsize` (unprivileged available)
/// - `used = total − bfree * frsize` (includes reserved for root)
pub fn capacity_from_statvfs(
    blocks: u64,
    bfree: u64,
    bavail: u64,
    frsize: u64,
) -> Option<(u64, u64, u64, f32)> {
    if frsize == 0 {
        return None;
    }
    let total = blocks.checked_mul(frsize)?;
    if total == 0 {
        return None;
    }
    let free_total = bfree.saturating_mul(frsize);
    let available = bavail.saturating_mul(frsize);
    let used = total.saturating_sub(free_total);
    let use_percent = (used as f64 / total as f64 * 100.0) as f32;
    Some((total, used, available, use_percent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_space_and_backslash() {
        assert_eq!(fstab_unescape(r"/mnt/my\040disk"), "/mnt/my disk");
        assert_eq!(fstab_unescape(r"a\134b"), "a\\b");
        assert_eq!(fstab_unescape(r"x\011y"), "x\ty");
    }

    #[test]
    fn unescape_preserves_utf8() {
        // Multi-byte UTF-8 must not be split into single-byte chars.
        assert_eq!(fstab_unescape("/mnt/café"), "/mnt/café");
        assert_eq!(fstab_unescape("/mnt/磁盘"), "/mnt/磁盘");
        assert_eq!(fstab_unescape(r"/mnt/café\040x"), "/mnt/café x");
    }

    #[test]
    fn unescape_rejects_non_octal_and_unknown() {
        // '8' is not an octal digit — leave the sequence literal.
        assert_eq!(fstab_unescape(r"a\080b"), r"a\080b");
        // Valid octal but not a standard fstab escape — leave literal.
        assert_eq!(fstab_unescape(r"a\101b"), r"a\101b"); // would be 'A'
    }

    #[test]
    fn parse_root_ext4() {
        let line = "/dev/mapper/data-root / ext4 rw,noatime 0 0";
        let e = parse_mounts_line(line).unwrap();
        assert_eq!(e.source, "/dev/mapper/data-root");
        assert_eq!(e.target, "/");
        assert_eq!(e.fstype, "ext4");
        assert!(is_health_mount(&e));
    }

    #[test]
    fn exclude_tmpfs_and_portal() {
        let tmp = parse_mounts_line("tmpfs /run tmpfs rw 0 0").unwrap();
        assert!(!is_health_mount(&tmp));
        let portal = parse_mounts_line("portal /run/user/1000/doc fuse.portal rw 0 0").unwrap();
        assert!(!is_health_mount(&portal));
        let efi_vars =
            parse_mounts_line("efivarfs /sys/firmware/efi/efivars efivarfs rw 0 0").unwrap();
        assert!(!is_health_mount(&efi_vars));
    }

    #[test]
    fn escaped_mountpoint_included() {
        let line = r"/dev/sdb1 /mnt/my\040disk ext4 rw 0 0";
        let e = parse_mounts_line(line).unwrap();
        assert_eq!(e.target, "/mnt/my disk");
        assert!(is_health_mount(&e));
    }

    #[test]
    fn parse_table_filters() {
        let text = "\
tmpfs /run tmpfs rw 0 0
/dev/mapper/root / ext4 rw 0 0
/dev/nvme0n1p1 /boot/efi vfat rw 0 0
portal /run/user/1000/doc fuse.portal rw 0 0
";
        let v = parse_health_mounts(text);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].target, "/");
        assert_eq!(v[1].target, "/boot/efi");
    }

    #[test]
    fn capacity_uses_bavail_and_reserved() {
        // 1000 blocks, 200 free, 100 avail to user, frsize 1024
        // used = 1000*1024 - 200*1024 = 800*1024
        let (total, used, avail, pct) = capacity_from_statvfs(1000, 200, 100, 1024).unwrap();
        assert_eq!(total, 1000 * 1024);
        assert_eq!(used, 800 * 1024);
        assert_eq!(avail, 100 * 1024);
        assert!((pct - 80.0).abs() < 0.01);
    }

    #[test]
    fn capacity_rejects_zero_frsize_or_total() {
        assert!(capacity_from_statvfs(100, 10, 10, 0).is_none());
        assert!(capacity_from_statvfs(0, 0, 0, 4096).is_none());
    }
}

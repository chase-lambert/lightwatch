//! NVMe SMART log parse and pure admin Get Log Page command builder.

/// NVMe Admin Get Log Page opcode.
pub const NVME_ADMIN_GET_LOG_PAGE: u8 = 0x02;
/// SMART / Health Information log page identifier.
pub const NVME_LOG_SMART: u32 = 0x02;
/// Controller-wide namespace id for SMART.
pub const NVME_NSID_ALL: u32 = 0xffff_ffff;
/// SMART log length in bytes.
pub const NVME_SMART_LOG_LEN: u32 = 512;
/// Zero-based number of dwords in the data buffer: 512/4 − 1.
pub const NVME_SMART_NUMDL: u32 = 127;
/// Bounded ioctl timeout.
pub const NVME_ADMIN_TIMEOUT_MS: u32 = 5_000;

/// `_IOWR('N', 0x41, struct nvme_admin_cmd)` with 72-byte payload.
pub const NVME_IOCTL_ADMIN_CMD: libc::c_ulong = 0xc048_4e41;

/// Linux `struct nvme_passthru_cmd` / `nvme_admin_cmd` (72 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvmeAdminCmd {
    pub opcode: u8,
    pub flags: u8,
    pub rsvd1: u16,
    pub nsid: u32,
    pub cdw2: u32,
    pub cdw3: u32,
    pub metadata: u64,
    pub addr: u64,
    pub metadata_len: u32,
    pub data_len: u32,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
    pub timeout_ms: u32,
    pub result: u32,
}

/// Parsed fields from a 512-byte SMART / Health log (NVMe).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NvmeSmartLog {
    pub critical_warning: u8,
    /// Composite temperature in degrees Celsius (from Kelvin in the log).
    pub temp_celsius: Option<i32>,
    pub percentage_used: u8,
    /// Full 16-byte LE media/data integrity error counter.
    pub media_errors: u128,
}

/// Build a Get Log Page admin command targeting the SMART log into `buf_ptr`.
///
/// Pure: no syscall. Tests lock field layout for the ioctl ABI.
pub fn build_smart_log_cmd(buf_ptr: u64) -> NvmeAdminCmd {
    // CDW10: LID in 7:0, NUMDL (zero-based DWORDs) in 31:16.
    let cdw10 = (NVME_SMART_NUMDL << 16) | (NVME_LOG_SMART & 0xff);
    NvmeAdminCmd {
        opcode: NVME_ADMIN_GET_LOG_PAGE,
        flags: 0,
        rsvd1: 0,
        nsid: NVME_NSID_ALL,
        cdw2: 0,
        cdw3: 0,
        metadata: 0,
        addr: buf_ptr,
        metadata_len: 0,
        data_len: NVME_SMART_LOG_LEN,
        cdw10,
        cdw11: 0,
        cdw12: 0,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
        timeout_ms: NVME_ADMIN_TIMEOUT_MS,
        result: 0,
    }
}

/// Controller character device path for a whole-disk name like `nvme0n1` → `/dev/nvme0`.
pub fn nvme_controller_dev(block_name: &str) -> Option<String> {
    // nvme0n1, nvme10n2, …
    let rest = block_name.strip_prefix("nvme")?;
    let n_idx = rest.find('n')?;
    let ctrl_num = &rest[..n_idx];
    if ctrl_num.is_empty() || !ctrl_num.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // remainder must be n<digits> with no partition `p`
    let ns = &rest[n_idx..];
    if !ns.starts_with('n') {
        return None;
    }
    let ns_num = &ns[1..];
    if ns_num.is_empty() || !ns_num.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!("/dev/nvme{ctrl_num}"))
}

/// Controller sysfs name `nvme0` from block name `nvme0n1`.
pub fn nvme_controller_name(block_name: &str) -> Option<String> {
    let dev = nvme_controller_dev(block_name)?;
    dev.strip_prefix("/dev/").map(|s| s.to_string())
}

fn read_u128_le_at(buf: &[u8], off: usize) -> Option<u128> {
    let slice = buf.get(off..off + 16)?;
    Some(u128::from_le_bytes(slice.try_into().ok()?))
}

/// Parse SMART log bytes. Requires at least 512 bytes for full field set;
/// returns None if shorter than minimum needed for percentage_used (6 bytes).
pub fn parse_nvme_smart_log(buf: &[u8]) -> Option<NvmeSmartLog> {
    if buf.len() < 6 {
        return None;
    }
    let critical_warning = buf[0];
    let temp_celsius = if buf.len() >= 3 {
        let kelvin = u16::from_le_bytes([buf[1], buf[2]]);
        if kelvin == 0 {
            None
        } else {
            Some(i32::from(kelvin) - 273)
        }
    } else {
        None
    };
    let percentage_used = buf[5];
    // Media and Data Integrity Errors: bytes 160..176 (16-byte LE counter).
    let media_errors = if buf.len() >= 176 {
        read_u128_le_at(buf, 160).unwrap_or(0)
    } else {
        0
    };
    Some(NvmeSmartLog {
        critical_warning,
        temp_celsius,
        percentage_used,
        media_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn admin_cmd_abi() {
        assert_eq!(size_of::<NvmeAdminCmd>(), 72);
        assert_eq!(align_of::<NvmeAdminCmd>(), 8);
        assert_eq!(offset_of!(NvmeAdminCmd, opcode), 0);
        assert_eq!(offset_of!(NvmeAdminCmd, nsid), 4);
        assert_eq!(offset_of!(NvmeAdminCmd, addr), 24);
        assert_eq!(offset_of!(NvmeAdminCmd, data_len), 36);
        assert_eq!(offset_of!(NvmeAdminCmd, cdw10), 40);
        assert_eq!(offset_of!(NvmeAdminCmd, timeout_ms), 64);
        assert_eq!(offset_of!(NvmeAdminCmd, result), 68);
        assert_eq!(NVME_IOCTL_ADMIN_CMD, 0xc048_4e41);
    }

    #[test]
    fn smart_cmd_fields() {
        let cmd = build_smart_log_cmd(0x1000);
        assert_eq!(cmd.opcode, NVME_ADMIN_GET_LOG_PAGE);
        assert_eq!(cmd.nsid, NVME_NSID_ALL);
        assert_eq!(cmd.addr, 0x1000);
        assert_eq!(cmd.data_len, 512);
        assert_eq!(cmd.cdw10, (127 << 16) | 0x02);
        assert_eq!(cmd.timeout_ms, NVME_ADMIN_TIMEOUT_MS);
        assert_eq!(cmd.flags, 0);
        assert_eq!(cmd.metadata_len, 0);
    }

    #[test]
    fn controller_dev_mapping() {
        assert_eq!(
            nvme_controller_dev("nvme0n1").as_deref(),
            Some("/dev/nvme0")
        );
        assert_eq!(
            nvme_controller_dev("nvme10n2").as_deref(),
            Some("/dev/nvme10")
        );
        assert_eq!(nvme_controller_dev("nvme0n1p1"), None);
        assert_eq!(nvme_controller_dev("sda"), None);
    }

    #[test]
    fn parse_smart_fixture() {
        let mut buf = vec![0u8; 512];
        buf[0] = 0x01; // critical warning bit
        // temp 310 K = 37 °C
        buf[1] = (310u16 & 0xff) as u8;
        buf[2] = (310u16 >> 8) as u8;
        buf[5] = 12; // percentage used
        // media errors = 3 at offset 160
        buf[160] = 3;
        let log = parse_nvme_smart_log(&buf).unwrap();
        assert_eq!(log.critical_warning, 1);
        assert_eq!(log.temp_celsius, Some(37));
        assert_eq!(log.percentage_used, 12);
        assert_eq!(log.media_errors, 3);
    }

    #[test]
    fn media_errors_high_half() {
        let mut buf = vec![0u8; 512];
        buf[5] = 1;
        // low 8 bytes zero; high 8 bytes = 1 → value 2^64
        buf[168] = 1;
        let log = parse_nvme_smart_log(&buf).unwrap();
        assert_eq!(log.media_errors, 1u128 << 64);
    }

    #[test]
    fn parse_short_buffer() {
        assert!(parse_nvme_smart_log(&[0u8; 5]).is_none());
    }
}

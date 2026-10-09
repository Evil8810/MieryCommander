//! Windows system calls: drives, free space and network shares.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows_sys::Win32::NetworkManagement::NetManagement::{MAX_PREFERRED_LENGTH, NetApiBufferFree};
use windows_sys::Win32::NetworkManagement::WNet::{NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection2W};
use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, NetShareEnum, SHARE_INFO_1};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_CDROM: u32 = 5;
/// The user already has a connection to this server with other credentials.
const ERROR_SESSION_CREDENTIAL_CONFLICT: u32 = 1219;

/// Start helper programs without flashing a console window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

/// A NUL-terminated UTF-16 string from Windows.
fn from_wide(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: Windows hands us a valid NUL-terminated string.
    unsafe {
        let mut n = 0;
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
}

#[derive(Clone, Debug)]
pub struct Drive {
    /// "C:\"
    pub root: PathBuf,
    /// "C: Windows"
    pub label: String,
    pub kind: DriveKind,
}

/// All drive letters with their kind and name.
pub fn drives() -> Vec<Drive> {
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let w = wide(&root);
        // SAFETY: `w` is a NUL-terminated path.
        let kind = match unsafe { GetDriveTypeW(w.as_ptr()) } {
            DRIVE_FIXED => DriveKind::Fixed,
            DRIVE_REMOVABLE | DRIVE_CDROM => DriveKind::Removable,
            DRIVE_REMOTE => DriveKind::Network,
            _ => continue,
        };
        // Network drives may hang when the server is gone: no name lookup for them.
        let name = if kind == DriveKind::Network { String::new() } else { volume_name(&w) };
        let label = if name.is_empty() { format!("{letter}:") } else { format!("{letter}: {name}") };
        out.push(Drive { root: PathBuf::from(root), label, kind });
    }
    out
}

fn volume_name(root: &[u16]) -> String {
    let mut buf = [0u16; 261];
    // SAFETY: `root` is NUL-terminated, `buf` is writable for its length; other outputs are optional.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 { String::new() } else { from_wide(buf.as_ptr()) }
}

/// Free and total bytes of the drive containing `path`.
pub fn disk_space(path: &Path) -> Option<(u64, u64)> {
    let w = wide(path);
    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY: `w` is NUL-terminated, outputs are valid pointers.
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
    (ok != 0).then_some((free, total))
}

/// Log in to `\\server\share` (empty user/password = the current Windows login).
pub fn connect_share(unc: &str, user: &str, password: &str) -> Result<(), String> {
    let mut remote = wide(unc);
    let res = NETRESOURCEW { dwType: RESOURCETYPE_DISK, lpRemoteName: remote.as_mut_ptr(), ..Default::default() };
    let user_w = (!user.trim().is_empty()).then(|| wide(user.trim()));
    let pass_w = (!password.is_empty()).then(|| wide(password));
    let ptr = |v: &Option<Vec<u16>>| v.as_ref().map_or(std::ptr::null(), |v| v.as_ptr());
    // SAFETY: all strings are NUL-terminated and live until the call returns.
    let r = unsafe { WNetAddConnection2W(&res, ptr(&pass_w), ptr(&user_w), 0) };
    match r {
        0 | ERROR_SESSION_CREDENTIAL_CONFLICT => Ok(()),
        e => Err(std::io::Error::from_raw_os_error(e as i32).to_string()),
    }
}

/// The normal (disk) shares of a server, without hidden ones like "C$".
pub fn list_shares(host: &str) -> Result<Vec<String>, String> {
    let server = wide(format!("\\\\{host}"));
    let mut buf: *mut u8 = std::ptr::null_mut();
    let (mut read, mut total) = (0u32, 0u32);
    // SAFETY: valid server name and out-pointers; the buffer is freed below.
    let r = unsafe { NetShareEnum(server.as_ptr(), 1, &mut buf, MAX_PREFERRED_LENGTH, &mut read, &mut total, std::ptr::null_mut()) };
    if r != 0 {
        return Err(std::io::Error::from_raw_os_error(r as i32).to_string());
    }
    let mut out = Vec::new();
    if !buf.is_null() {
        // SAFETY: NetShareEnum returned `read` SHARE_INFO_1 records in `buf`.
        let items = unsafe { std::slice::from_raw_parts(buf as *const SHARE_INFO_1, read as usize) };
        for it in items {
            let name = from_wide(it.shi1_netname);
            // Type 0 = disk; special (admin) shares have the high bit set.
            if it.shi1_type == 0 && !name.ends_with('$') {
                out.push(name);
            }
        }
        // SAFETY: the buffer came from NetShareEnum.
        unsafe { NetApiBufferFree(buf as *const _) };
    }
    out.sort_by_key(|s| s.to_lowercase());
    Ok(out)
}

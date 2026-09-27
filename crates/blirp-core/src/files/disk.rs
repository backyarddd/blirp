//! Disk facts the file engine needs: free space (the hub's default quota)
//! and whether a folder is on a removable or network drive (never synced in
//! v1: such a drive may be gone or shared with other machines).

use std::path::Path;

/// Where a folder lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    Local,
    Removable,
    Network,
}

/// Bytes available to this user on the filesystem holding `path`.
pub fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `c` is a valid NUL-terminated string that outlives the
        // call, and `st` is a properly sized out-parameter that statvfs
        // fully initializes when it returns 0.
        #[allow(unsafe_code)]
        let st = unsafe {
            let mut st: libc::statvfs = std::mem::zeroed();
            (libc::statvfs(c.as_ptr(), &mut st) == 0).then_some(st)
        }?;
        #[allow(clippy::unnecessary_cast)]
        Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut avail: u64 = 0;
        // SAFETY: `wide` is NUL-terminated and outlives the call; the out
        // pointers are either valid u64s or null, as the API allows.
        #[allow(unsafe_code)]
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut avail,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(avail)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
/// Filesystem types that are network shares (Linux `/proc/self/mounts`).
const NETWORK_FS: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "sshfs",
    "fuse.sshfs",
    "9p",
    "afs",
    "ceph",
    "glusterfs",
    "davfs",
    "fuse.rclone",
    "fuse.s3fs",
];

#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
/// Mount type of the longest mount point containing `path` in `mounts`
/// (`/proc/self/mounts` format).
fn mount_type<'a>(mounts: &'a str, path: &Path) -> Option<&'a str> {
    mounts
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (_, point, kind) = (f.next()?, f.next()?, f.next()?);
            // Spaces in mount points are escaped as \040.
            let point = point.replace("\\040", " ");
            path.starts_with(&point).then_some((point.len(), kind))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, kind)| kind)
}

/// Whether `path` is on a removable or network drive (best effort; unknown
/// counts as local).
pub fn drive_kind(path: &Path) -> DriveKind {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let s = path.to_string_lossy();
        if s.starts_with(r"\\") && !s.starts_with(r"\\?\") {
            return DriveKind::Network;
        }
        let Some(std::path::Component::Prefix(prefix)) = path.components().next() else {
            return DriveKind::Local;
        };
        let mut root = prefix.as_os_str().to_os_string();
        root.push("\\");
        let wide: Vec<u16> = root.encode_wide().chain(Some(0)).collect();
        // SAFETY: `wide` is a NUL-terminated root path that outlives the call.
        #[allow(unsafe_code)]
        let t = unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) };
        match t {
            2 | 5 => DriveKind::Removable, // DRIVE_REMOVABLE, DRIVE_CDROM
            4 => DriveKind::Network,       // DRIVE_REMOTE
            _ => DriveKind::Local,
        }
    }
    #[cfg(target_os = "macos")]
    {
        // External disks and mounted shares live under /Volumes.
        if path.starts_with("/Volumes") {
            return DriveKind::Removable;
        }
        DriveKind::Local
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if path.starts_with("/media") || path.starts_with("/run/media") {
            return DriveKind::Removable;
        }
        match std::fs::read_to_string("/proc/self/mounts") {
            Ok(m) if mount_type(&m, path).is_some_and(|t| NETWORK_FS.contains(&t)) => {
                DriveKind::Network
            }
            _ => DriveKind::Local,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        DriveKind::Local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_types_pick_the_longest_prefix() {
        let mounts = "/dev/sda1 / ext4 rw 0 0\nserver:/x /home/u/net nfs4 rw 0 0\n/dev/sdb1 /home/u/my\\040disk ext4 rw 0 0\n";
        assert_eq!(mount_type(mounts, Path::new("/home/u/net/p")), Some("nfs4"));
        assert_eq!(mount_type(mounts, Path::new("/home/u/code")), Some("ext4"));
        assert_eq!(
            mount_type(mounts, Path::new("/home/u/my disk/p")),
            Some("ext4")
        );
        assert!(NETWORK_FS.contains(&"nfs4"));
    }

    #[test]
    fn this_machine_has_free_space_and_a_local_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_bytes(dir.path()).is_some_and(|b| b > 0));
        assert_eq!(drive_kind(dir.path()), DriveKind::Local);
    }
}

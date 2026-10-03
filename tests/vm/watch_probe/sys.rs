use std::ffi::CString;
use std::fs::{self};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::fan;

// ---------------------------------------------------------------------------
// Small syscall wrappers
// ---------------------------------------------------------------------------

pub(crate) fn cpath(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).expect("path without NUL")
}

pub(crate) fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

pub(crate) fn ename(e: i32) -> String {
    let name = match e {
        libc::EPERM => "EPERM",
        libc::ENOENT => "ENOENT",
        libc::EBADF => "EBADF",
        libc::EACCES => "EACCES",
        libc::EEXIST => "EEXIST",
        libc::EXDEV => "EXDEV",
        libc::ENODEV => "ENODEV",
        libc::ENOTDIR => "ENOTDIR",
        libc::EINVAL => "EINVAL",
        libc::ENFILE => "ENFILE",
        libc::EMFILE => "EMFILE",
        libc::ENOSPC => "ENOSPC",
        libc::ENOSYS => "ENOSYS",
        libc::EOPNOTSUPP => "EOPNOTSUPP",
        libc::ESTALE => "ESTALE",
        libc::EAGAIN => "EAGAIN",
        libc::EFAULT => "EFAULT",
        libc::EOVERFLOW => "EOVERFLOW",
        libc::ELOOP => "ELOOP",
        _ => return format!("errno {e}"),
    };
    name.to_string()
}

pub(crate) fn io_err(e: &std::io::Error) -> String {
    e.raw_os_error().map(ename).unwrap_or_else(|| e.to_string())
}

pub(crate) fn ok_or<T>(r: Result<T, i32>) -> String {
    match r {
        Ok(_) => "OK".into(),
        Err(e) => ename(e),
    }
}

pub(crate) fn fan_init(flags: u32, event_f_flags: u32) -> Result<OwnedFd, i32> {
    let fd = unsafe { libc::fanotify_init(flags, event_f_flags) };
    if fd < 0 {
        Err(last_errno())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

pub(crate) fn design_group() -> OwnedFd {
    fan_init(fan::DESIGN_INIT, (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_LARGEFILE) as u32)
        .unwrap_or_else(|e| panic!("the design's fanotify_init failed: {}", ename(e)))
}

pub(crate) fn fan_mark(group: &OwnedFd, flags: u32, mask: u64, path: &Path) -> Result<(), i32> {
    let c = cpath(path);
    let rc =
        unsafe { libc::fanotify_mark(group.as_raw_fd(), flags, mask, libc::AT_FDCWD, c.as_ptr()) };
    if rc < 0 {
        Err(last_errno())
    } else {
        Ok(())
    }
}

#[repr(C)]
struct RawHandle {
    bytes: u32,
    htype: i32,
    data: [u8; 128],
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Handle {
    pub(crate) htype: i32,
    pub(crate) data: Vec<u8>,
}

impl Handle {
    pub(crate) fn show(&self) -> String {
        let hex: String = self.data.iter().map(|b| format!("{b:02x}")).collect();
        format!("type {:#x}, {} bytes, {hex}", self.htype, self.data.len())
    }
}

/// `name_to_handle_at` on a path, not following a final symlink.
pub(crate) fn name_to_handle(path: &Path, flags: i32) -> Result<(Handle, i32), i32> {
    let c = cpath(path);
    let mut raw = RawHandle { bytes: 128, htype: 0, data: [0; 128] };
    let mut mount_id: libc::c_int = 0;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_name_to_handle_at,
            libc::AT_FDCWD,
            c.as_ptr(),
            &mut raw as *mut RawHandle,
            &mut mount_id as *mut libc::c_int,
            flags,
        )
    };
    if rc < 0 {
        return Err(last_errno());
    }
    Ok((
        Handle { htype: raw.htype, data: raw.data[..raw.bytes as usize].to_vec() },
        mount_id,
    ))
}

pub(crate) fn open_by_handle(mount_fd: RawFd, h: &Handle, flags: i32) -> Result<OwnedFd, i32> {
    let mut raw = RawHandle { bytes: h.data.len() as u32, htype: h.htype, data: [0; 128] };
    raw.data[..h.data.len()].copy_from_slice(&h.data);
    let rc = unsafe {
        libc::syscall(libc::SYS_open_by_handle_at, mount_fd, &mut raw as *mut RawHandle, flags)
    };
    if rc < 0 {
        Err(last_errno())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(rc as i32) })
    }
}

pub(crate) fn statfs_of(path: &Path) -> libc::statfs {
    let c = cpath(path);
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c.as_ptr(), &mut st) };
    assert_eq!(rc, 0, "statfs {path:?}: {}", ename(last_errno()));
    st
}

pub(crate) fn fsid_of(path: &Path) -> [i32; 2] {
    let st = statfs_of(path);
    // `fsid_t`'s field is private in the libc crate; it is two ints.
    unsafe { *(&st.f_fsid as *const libc::fsid_t as *const [i32; 2]) }
}

pub(crate) fn show_fsid(f: [i32; 2]) -> String {
    format!("{:08x}:{:08x}", f[0] as u32, f[1] as u32)
}

pub(crate) fn read_sysctl(name: &str) -> String {
    fs::read_to_string(format!("/proc/sys/fs/fanotify/{name}"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|e| format!("<{}>", io_err(&e)))
}

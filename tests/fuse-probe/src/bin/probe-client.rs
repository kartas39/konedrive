//! What a program does to a file in the mount: a store through a shared mapping, ended one of
//! several ways. `probe-client mmap <file> <msync|munmap|closefirst|exit|kill>`.

use std::ffi::CString;

const LEN: usize = 1 << 20;

fn now_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    assert!(a.len() == 4 && a[1] == "mmap", "probe-client mmap <file> <variant>");
    let path = CString::new(a[2].as_str()).unwrap();
    let variant = a[3].as_str();
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o644);
        assert!(fd >= 0, "open: {}", std::io::Error::last_os_error());
        assert_eq!(libc::ftruncate(fd, LEN as i64), 0);
        let p = libc::mmap(std::ptr::null_mut(), LEN, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
        assert!(p != libc::MAP_FAILED, "mmap: {}", std::io::Error::last_os_error());
        if variant != "msync" {
            libc::close(fd);
        }
        std::ptr::write_bytes(p as *mut u8, b'Z', LEN);
        println!("{} client: stored", now_ms());
        match variant {
            "msync" => {
                assert_eq!(libc::msync(p, LEN, libc::MS_SYNC), 0);
                println!("{} client: msync returned", now_ms());
                libc::munmap(p, LEN);
                libc::close(fd);
            }
            "munmap" | "closefirst" => {
                libc::munmap(p, LEN);
                println!("{} client: munmap returned", now_ms());
            }
            "exit" => libc::_exit(0),
            "kill" => {
                libc::kill(libc::getpid(), libc::SIGKILL);
            }
            _ => panic!("unknown variant"),
        }
        println!("{} client: done", now_ms());
    }
}

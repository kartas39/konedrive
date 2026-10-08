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
    assert!(a.len() == 4, "probe-client mmap <file> <variant> | watch <dir> <seconds>");
    if a[1] == "watch" {
        return watch(&a[2], a[3].parse().unwrap());
    }
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

/// What a file manager does: an inotify watch on a directory, every event printed.
fn watch(dir: &str, secs: i32) {
    unsafe {
        let fd = libc::inotify_init1(0);
        let mask = libc::IN_CREATE | libc::IN_DELETE | libc::IN_ATTRIB | libc::IN_MODIFY | libc::IN_MOVED_FROM | libc::IN_MOVED_TO;
        assert!(libc::inotify_add_watch(fd, CString::new(dir).unwrap().as_ptr(), mask) >= 0);
        let end = now_ms() + secs as u64 * 1000;
        let mut buf = [0u8; 4096];
        while now_ms() < end {
            let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            if libc::poll(&mut p, 1, 200) <= 0 {
                continue;
            }
            let n = libc::read(fd, buf.as_mut_ptr().cast(), buf.len());
            let mut off = 0;
            while off < n as usize {
                let ev = &*(buf.as_ptr().add(off) as *const libc::inotify_event);
                let name = std::ffi::CStr::from_ptr(buf.as_ptr().add(off + 16).cast()).to_string_lossy();
                let kind = [(libc::IN_CREATE, "CREATE"), (libc::IN_DELETE, "DELETE"), (libc::IN_ATTRIB, "ATTRIB"), (libc::IN_MODIFY, "MODIFY")]
                    .iter()
                    .filter(|(m, _)| ev.mask & m != 0)
                    .map(|(_, n)| *n)
                    .collect::<Vec<_>>()
                    .join("|");
                println!("inotify: {kind} {name}");
                off += 16 + ev.len as usize;
            }
        }
    }
}

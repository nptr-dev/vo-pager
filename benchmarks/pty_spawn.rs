//! Keep the Python interpreter out of the measured pager child's peak RSS.
//!
//! Linux retains peak RSS across exec. This small native parent reports wait4
//! usage for the pager itself through a separate descriptor, leaving terminal
//! output unchanged. The benchmark launcher already owns the controlling PTY.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::FromRawFd;
use std::os::raw::{c_int, c_long};
use std::process::Command;

#[repr(C)]
#[derive(Default)]
struct Timeval {
    seconds: c_long,
    microseconds: c_long,
}

#[repr(C)]
#[derive(Default)]
struct Rusage {
    user: Timeval,
    system: Timeval,
    max_rss_kib: c_long,
    other_fields: [c_long; 13],
}

unsafe extern "C" {
    fn wait4(pid: c_int, status: *mut c_int, options: c_int, usage: *mut Rusage) -> c_int;
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
}

fn run() -> io::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let report_fd: c_int = arguments
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "expected report descriptor"))?;
    let executable = arguments
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "expected pager executable"))?;
    // F_SETFD = 2, FD_CLOEXEC = 1 on Linux. The pager must not retain the
    // measurement pipe because that would delay report EOF until pager exit.
    if unsafe { fcntl(report_fd, 2, 1) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut report = unsafe { File::from_raw_fd(report_fd) };
    let result = (|| {
        let child = Command::new(executable).args(arguments).spawn()?;
        let pid = child.id() as c_int;
        writeln!(report, "PID {pid}")?;
        let mut status = 0;
        let mut usage = Rusage::default();
        loop {
            let result = unsafe { wait4(pid, &mut status, 0, &mut usage) };
            if result == pid {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        writeln!(report, "EXIT {status} {}", usage.max_rss_kib * 1024)
    })();
    if let Err(ref error) = result {
        let _ = writeln!(report, "ERROR {error}");
    }
    result
}

fn main() {
    if let Err(error) = run() {
        eprintln!("pager measurement supervisor failed: {error}");
        std::process::exit(1);
    }
}

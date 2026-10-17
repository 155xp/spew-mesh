//! Client process checks for session lifetime rule 2 (6.4): a pid with the start time recorded when the session
//! opened. A pid whose start time differs belongs to another process, so the client has exited.

/// Reads a process's start time as an opaque number (clock ticks on Linux, microseconds on macOS), or `None` when
/// there is no such process or it can't be read.
pub fn start_time(pid: u32) -> Option<u64> {
    imp::start_time(pid)
}

/// Whether the process `pid` still runs and, when a start time was recorded, is the same process.
pub fn is_alive(
    pid: u32,
    recorded_start: Option<u64>,
    start_time: &dyn Fn(u32) -> Option<u64>,
) -> bool {
    match (start_time(pid), recorded_start) {
        (None, _) => false,
        (Some(now), Some(then)) => now == then,
        (Some(_), None) => true,
    }
}

/// Field 22 (`starttime`) of a `/proc/<pid>/stat` line. The command name sits in parentheses and may hold spaces or
/// parentheses itself, so fields are counted after the last `)`.
pub fn parse_proc_stat(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // After the name come field 3 (state) onwards; starttime is field 22.
    rest.split_whitespace().nth(22 - 3)?.parse().ok()
}

#[cfg(target_os = "linux")]
mod imp {
    pub fn start_time(pid: u32) -> Option<u64> {
        if pid == 0 {
            return None;
        }
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        super::parse_proc_stat(&stat)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    #[allow(unsafe_code)]
    pub fn start_time(pid: u32) -> Option<u64> {
        let pid = libc::c_int::try_from(pid).ok().filter(|p| *p > 0)?;
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: `info` is a properly sized, writable proc_bsdinfo and `size` is its exact size, as
        // proc_pidinfo(PROC_PIDTBSDINFO) requires; the call writes at most `size` bytes.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        if written != size {
            return None;
        }
        Some(info.pbi_start_tvsec.saturating_mul(1_000_000) + info.pbi_start_tvusec)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    pub fn start_time(_pid: u32) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_lines_with_odd_names() {
        let line = "4242 (my (odd) name) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
        assert_eq!(parse_proc_stat(line), Some(987654));
        assert_eq!(parse_proc_stat("garbage"), None);
        assert_eq!(parse_proc_stat("1 (x) S 1"), None);
    }

    #[test]
    fn a_changed_start_time_means_exited() {
        let fixed = |_: u32| Some(10);
        assert!(is_alive(5, Some(10), &fixed));
        assert!(!is_alive(5, Some(9), &fixed));
        assert!(is_alive(5, None, &fixed));
        assert!(!is_alive(5, Some(10), &|_| None));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_has_a_start_time() {
        let me = std::process::id();
        let first = start_time(me);
        assert!(first.is_some());
        assert_eq!(first, start_time(me));
        assert!(is_alive(me, first, &start_time));
    }
}

/// Read process resident memory (RSS) in bytes from Linux procfs.
#[cfg(target_os = "linux")]
pub fn read_process_resident_memory_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?; // determinism-ok: procfs RSS read for observability only
    let vm_rss_line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let mut parts = vm_rss_line.split_whitespace();
    let _label = parts.next()?;
    let value_kb = parts.next()?.parse::<u64>().ok()?;
    Some(value_kb.saturating_mul(1024))
}

/// Read process resident memory (RSS) in bytes from Linux procfs.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
pub fn read_process_resident_memory_bytes() -> Option<u64> {
    use std::ptr;

    let mut info = libc::mach_task_basic_info {
        virtual_size: 0,
        resident_size: 0,
        resident_size_max: 0,
        user_time: libc::time_value_t {
            seconds: 0,
            microseconds: 0,
        },
        system_time: libc::time_value_t {
            seconds: 0,
            microseconds: 0,
        },
        policy: 0,
        suspend_count: 0,
    };
    let mut count = libc::MACH_TASK_BASIC_INFO_COUNT;

    // determinism-ok: local task_info call for observability only
    let status = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            libc::MACH_TASK_BASIC_INFO,
            ptr::addr_of_mut!(info).cast::<libc::integer_t>(),
            &mut count,
        )
    };

    if status == libc::KERN_SUCCESS {
        Some(info.resident_size)
    } else {
        None
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn read_process_resident_memory_bytes() -> Option<u64> {
    None
}

//! The memory this host leaves the joint walks of the specs it is sent.
//!
//! A joint walk holds every state it has seen, so a model that never ends
//! fills the host until the host kills the process, with every tenant on it.
//! The walks of one process share [`HOST_ROOM`]: together they hold at most
//! half of the memory the host leaves free to them. Free is what every
//! process sharing a limit with this one leaves, not just this server: one
//! sandbox runs many app servers under one cgroup, and the kernel kills
//! whichever crosses it. Half, because a walk's count of what it holds is
//! an estimate (the allocator rounds and fragments), and the server keeps
//! the rest to go on serving while a walk runs. Free memory is read again
//! each time a walk asks for more, so the room follows the host as it
//! changes.

use std::sync::atomic::{AtomicUsize, Ordering};

use temper_verify::composite::Room;

const MB: usize = 1 << 20;

/// The memory this process's joint walks hold.
pub struct HostRoom {
    held: AtomicUsize,
}

/// The one room the joint walks of this process share.
pub static HOST_ROOM: HostRoom = HostRoom {
    held: AtomicUsize::new(0),
};

impl HostRoom {
    /// The bytes joint walks may still take now: half of what the host
    /// leaves free to them, less what they hold. `None` when this host's
    /// free memory cannot be read.
    pub fn left(&self) -> Option<usize> {
        let held = self.held.load(Ordering::Relaxed);
        let free = free_memory()?;
        Some((free.saturating_add(held) / 2).saturating_sub(held))
    }
}

impl Room for HostRoom {
    fn take(&self, bytes: usize) -> Result<(), String> {
        let mut held = self.held.load(Ordering::Relaxed);
        loop {
            let free = free_memory()
                .ok_or_else(|| "this host's free memory could not be read".to_string())?;
            // The walks' own memory is counted as used in `free`.
            let room = free.saturating_add(held) / 2;
            let after = held.saturating_add(bytes);
            if after > room {
                return Err(format!(
                    "this host leaves joint walks {} MB (half of the {} MB free to them) \
                     and they hold {} MB",
                    room / MB,
                    free.saturating_add(held) / MB,
                    held / MB
                ));
            }
            match self
                .held
                .compare_exchange(held, after, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Ok(()),
                Err(now) => held = now,
            }
        }
    }

    fn give(&self, bytes: usize) {
        self.held.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// The memory this process can still take: the least of what the machine
/// leaves available and what each cgroup above it with a limit leaves,
/// counting reclaimable file cache as free, as the kernel does.
#[cfg(target_os = "linux")]
fn free_memory() -> Option<usize> {
    use std::path::Path;

    let read = |path: &Path| std::fs::read_to_string(path).ok(); // determinism-ok: host memory read
    let mut free = field(&read(Path::new("/proc/meminfo"))?, "MemAvailable:")?.saturating_mul(1024);
    // Both versions may mount only a subtree. Resolve the process's group
    // through mountinfo before checking every visible ancestor's limit.
    if let (Some(groups), Some(mounts)) = (
        read(Path::new("/proc/self/cgroup")),
        read(Path::new("/proc/self/mountinfo")),
    ) {
        free = cgroup_free(&read, &groups, &mounts, free, true);
        free = cgroup_free(&read, &groups, &mounts, free, false);
    }
    Some(free)
}

/// Apply every visible memory limit from this process up to its mount.
#[cfg(target_os = "linux")]
fn cgroup_free(
    read: &impl Fn(&std::path::Path) -> Option<String>,
    groups: &str,
    mounts: &str,
    mut free: usize,
    v2: bool,
) -> usize {
    use std::path::Path;

    let own = groups.lines().find_map(|line| {
        let mut parts = line.splitn(3, ':');
        let hierarchy = parts.next()?;
        let controllers = parts.next()?;
        if v2 {
            return (hierarchy == "0" && controllers.is_empty())
                .then(|| parts.next())
                .flatten();
        }
        controllers
            .split(',')
            .any(|c| c == "memory")
            .then(|| parts.next())
            .flatten()
    });
    let Some(own) = own else { return free };
    // mountinfo escapes whitespace and backslashes in paths.
    let unescape = |path: &str| {
        path.replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\")
    };
    let files = if v2 {
        ["memory.max", "memory.current", "inactive_file"]
    } else {
        [
            "memory.limit_in_bytes",
            "memory.usage_in_bytes",
            "total_inactive_file",
        ]
    };
    for line in mounts.lines() {
        let Some((mount, kind)) = line.split_once(" - ") else {
            continue;
        };
        let kind: Vec<_> = kind.split_whitespace().collect();
        let matches = if v2 {
            kind.first() == Some(&"cgroup2")
        } else {
            kind.first() == Some(&"cgroup")
                && kind
                    .get(2)
                    .is_some_and(|options| options.split(',').any(|c| c == "memory"))
        };
        if !matches {
            continue;
        }
        let mount: Vec<_> = mount.split_whitespace().collect();
        let (Some(base), Some(root)) = (mount.get(3), mount.get(4)) else {
            continue;
        };
        let base = unescape(base);
        let root = unescape(root);
        let Ok(relative) = Path::new(own).strip_prefix(&base) else {
            continue;
        };
        let own = Path::new(&root).join(relative);
        for dir in own.ancestors().take_while(|dir| dir.starts_with(&root)) {
            if let Some(left) = group_free(read, dir, files) {
                free = free.min(left);
            }
        }
    }
    free
}

/// What the cgroup at `dir` leaves under its limit, from its limit, usage
/// and inactive file cache files.
#[cfg(target_os = "linux")]
fn group_free(
    read: &impl Fn(&std::path::Path) -> Option<String>,
    dir: &std::path::Path,
    [limit, usage, inactive]: [&str; 3],
) -> Option<usize> {
    let limit: usize = read(&dir.join(limit))?.trim().parse().ok()?;
    let usage: usize = read(&dir.join(usage))?.trim().parse().ok()?;
    let inactive = field(&read(&dir.join("memory.stat"))?, inactive)?;
    Some(limit.saturating_sub(usage.saturating_sub(inactive)))
}

/// The number after `name` on its line of a procfs or cgroup stat file.
#[cfg(target_os = "linux")]
fn field(text: &str, name: &str) -> Option<usize> {
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.split_whitespace().next())?
        .parse()
        .ok()
}

/// Pages macOS currently leaves free or purgeable without swapping.
/// Inactive pages can still hold non-purgeable application memory, so the
/// whole inactive list is not an allowance for a new proof.
#[cfg(target_os = "macos")]
fn free_memory() -> Option<usize> {
    // libc exposes the statistics layout, but not these Mach port APIs.
    unsafe extern "C" {
        fn mach_host_self() -> libc::mach_port_t;
        static mach_task_self_: libc::mach_port_t;
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
        fn host_page_size(
            host: libc::mach_port_t,
            size: *mut libc::vm_size_t,
        ) -> libc::kern_return_t;
    }

    let mut stats = std::mem::MaybeUninit::<libc::vm_statistics64_data_t>::zeroed();
    let mut count = (size_of::<libc::vm_statistics64_data_t>() / size_of::<libc::integer_t>())
        as libc::mach_msg_type_number_t;
    let mut page_size: libc::vm_size_t = 0;
    // SAFETY: both calls receive writable buffers of the sizes their APIs
    // require. The statistics buffer is read only after a successful call.
    let status = unsafe {
        let host = mach_host_self();
        if host_page_size(host, &mut page_size) != libc::KERN_SUCCESS {
            mach_port_deallocate(mach_task_self_, host);
            return None;
        }
        let status = libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            stats.as_mut_ptr().cast(),
            &mut count,
        );
        mach_port_deallocate(mach_task_self_, host);
        status
    };
    if status != libc::KERN_SUCCESS {
        return None;
    }
    // SAFETY: the buffer was zeroed and host_statistics64 succeeded.
    let stats = unsafe { stats.assume_init() };
    (stats.free_count as usize)
        .checked_add(stats.purgeable_count as usize)?
        .checked_mul(page_size as usize)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn free_memory() -> Option<usize> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    #[test]
    fn v2_checks_own_group_and_ancestors_on_subtree_mounts() {
        let groups = "0::/slice/tenant/server\n";
        for (base, root, mount_root) in [
            ("/slice", "/sys/fs/cgroup", "/sys/fs/cgroup"),
            ("/slice", "/memory mount", "/memory\\040mount"),
            ("/", "/sys/fs/cgroup/slice", "/sys/fs/cgroup"),
        ] {
            let mut files = BTreeMap::new();
            for (suffix, limit, usage, cache) in [
                ("", "max", 0, 0),
                ("/tenant", "4096", 100, 50),
                ("/tenant/server", "300", 100, 20),
            ] {
                let dir = format!("{root}{suffix}");
                files.insert(Path::new(&dir).join("memory.max"), limit.into());
                files.insert(Path::new(&dir).join("memory.current"), usage.to_string());
                files.insert(
                    Path::new(&dir).join("memory.stat"),
                    format!("inactive_file {cache}\n"),
                );
            }
            let mounts = format!("30 20 0:1 {base} {mount_root} rw - cgroup2 cgroup rw\n");
            let read = |path: &Path| files.get(path).cloned();
            assert_eq!(cgroup_free(&read, groups, &mounts, 8000, true), 220);
            assert_eq!(cgroup_free(&read, groups, &mounts, 10, true), 10);
            // A shared ancestor can be tighter than the process's own group.
            files.insert(Path::new(root).join("tenant/memory.max"), "150".into());
            let read = |path: &Path| files.get(path).cloned();
            assert_eq!(cgroup_free(&read, groups, &mounts, 8000, true), 100);
            assert_eq!(
                cgroup_free(&read, "0::/other/server", &mounts, 8000, true),
                8000
            );
        }
    }

    #[test]
    fn v1_checks_own_group_and_ancestors_on_a_subtree_mount() {
        let mut files = BTreeMap::new();
        for (dir, limit, usage, cache) in [
            ("/memory mount", 900, 100, 0),
            ("/memory mount/tenant", 500, 400, 50),
            ("/memory mount/tenant/server", 300, 100, 0),
        ] {
            files.insert(
                Path::new(dir).join("memory.limit_in_bytes"),
                limit.to_string(),
            );
            files.insert(
                Path::new(dir).join("memory.usage_in_bytes"),
                usage.to_string(),
            );
            files.insert(
                Path::new(dir).join("memory.stat"),
                format!("total_inactive_file {cache}\n"),
            );
        }
        let read = |path: &Path| files.get(path).cloned();
        let groups = "2:cpu:/elsewhere\n4:memory,other:/slice/tenant/server\n";
        let mounts = "30 20 0:1 /slice /memory\\040mount rw - cgroup cgroup rw,memory\n";
        assert_eq!(cgroup_free(&read, groups, mounts, 1000, false), 150);
        // The process's own group can be tighter than its shared ancestor.
        files.insert(
            Path::new("/memory mount/tenant/server/memory.limit_in_bytes").to_path_buf(),
            "120".into(),
        );
        let read = |path: &Path| files.get(path).cloned();
        assert_eq!(cgroup_free(&read, groups, mounts, 1000, false), 20);
        assert_eq!(cgroup_free(&read, groups, mounts, 10, false), 10);
    }
}

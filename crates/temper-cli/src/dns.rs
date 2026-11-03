//! How long a DNS lookup waits for a lost answer before asking again.
//!
//! glibc's resolver asks over UDP and, with no options in `/etc/resolv.conf`,
//! waits 5 s for the answer before asking again, so one lost packet holds
//! whatever waits on that lookup for 5 s. Tensorlake sandboxes resolve through
//! one nameserver with no options. On 28 Oct 2026, 37 synced store opens there
//! took 5.1 to 5.4 s instead of 0.3 to 1.5 s, each held on a lookup of its
//! Turso remote. A packet capture in the same kind of sandbox showed the cause:
//! an A answer that never arrived, asked again 5.005 s later.
//!
//! The median answer there took under 10 ms (12,400 lookups). Asking again
//! after 1 s, up to 3 times, makes a lost answer cost 1 s (all 28 asked again
//! in 10,000 lookups were answered within 1.03 s), and a late answer to an
//! earlier ask is still taken. Both HTTP clients of a synced store resolve through
//! glibc, and the Turso engine's client takes no resolver of its own, so this
//! is set for the whole process through `RES_OPTIONS`, which glibc reads over
//! the options in `resolv.conf`. An operator's own `RES_OPTIONS` is kept.

/// Ask again 1 s after a lost answer, at most 3 times.
const ASK_AGAIN_AFTER_ONE_SECOND: &str = "timeout:1 attempts:3";

/// Set `RES_OPTIONS` for this process unless it is already set.
///
/// Call it before any other thread starts: changing the environment while
/// another thread reads it is undefined behavior.
pub fn ask_again_after_one_second() {
    if std::env::var_os("RES_OPTIONS").is_none() {
        // SAFETY: `main` calls this before it builds the runtime, while this
        // is the only thread.
        unsafe { std::env::set_var("RES_OPTIONS", ASK_AGAIN_AFTER_ONE_SECOND) };
    }
}

#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
mod tests {
    use std::ffi::c_int;
    use std::process::Command;

    /// The first two fields of glibc's `struct __res_state`: seconds to wait
    /// for an answer before asking again, and how many times to ask.
    #[repr(C)]
    struct ResolverHead {
        retrans: c_int,
        retry: c_int,
    }

    unsafe extern "C" {
        fn __res_init() -> c_int;
        fn __res_state() -> *mut ResolverHead;
    }

    /// Set in the child process this test starts; holds the expected
    /// `retrans retry`.
    const CHILD_EXPECTS: &str = "TEMPER_DNS_TEST_CHILD_EXPECTS";
    const THIS_TEST: &str = "dns::tests::a_lost_dns_answer_is_asked_again_after_one_second";

    /// Run this test again in a fresh process, where setting the environment
    /// is safe, with `RES_OPTIONS` as given; the child checks glibc's state.
    fn in_child(res_options: Option<&str>, expects: &str) {
        let mut child = Command::new(std::env::current_exe().expect("test binary"));
        child
            .args(["--exact", THIS_TEST, "--test-threads=1", "--nocapture"])
            .env(CHILD_EXPECTS, expects);
        match res_options {
            Some(options) => child.env("RES_OPTIONS", options),
            None => child.env_remove("RES_OPTIONS"),
        };
        let out = child.output().expect("run the child test");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "child with RES_OPTIONS {res_options:?} failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_lost_dns_answer_is_asked_again_after_one_second() {
        if let Some(expects) = std::env::var_os(CHILD_EXPECTS) {
            super::ask_again_after_one_second();
            // SAFETY: glibc's own resolver state for this thread, read after
            // `res_init` filled it in; only its first two fields are read.
            let (retrans, retry) = unsafe {
                assert_eq!(__res_init(), 0, "res_init failed");
                let head = &*__res_state();
                (head.retrans, head.retry)
            };
            assert_eq!(
                format!("{retrans} {retry}"),
                expects.to_string_lossy(),
                "glibc's resolver waits {retrans} s for an answer and asks {retry} times"
            );
            return;
        }
        in_child(None, "1 3");
        // An operator's own setting wins.
        in_child(Some("timeout:2 attempts:4"), "2 4");
    }
}

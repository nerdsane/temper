//! Logging after the telemetry has shut down.

use crate::*;

/// `temper serve` shuts its telemetry down when it is asked to stop, and its
/// other tasks can still log after that. The process must still exit cleanly,
/// and the line must still reach its own log.
///
/// The SDK warns about every record it is given after shutdown. When the log
/// bridge exported that warning as one more record, each warning made another
/// until the stack overflowed and the process aborted: on 28 Oct 2026 every
/// stop of a Temper on roll 6 ended in SIGABRT.
#[test]
fn a_line_logged_after_shutdown_lets_the_process_exit_cleanly() {
    // `run_then_log_after_shutdown` fails unless the child exits successfully.
    let run = child::run_then_log_after_shutdown(&[]);
    let after: Vec<String> = log_messages(&run)
        .into_iter()
        .filter(|message| message == child::AFTER_SHUTDOWN_LINE)
        .collect();
    assert_eq!(
        after.len(),
        1,
        "the line logged after shutdown, in {:?}",
        log_messages(&run)
    );
}

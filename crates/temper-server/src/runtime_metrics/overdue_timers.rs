use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Gauge;
use std::sync::OnceLock;

/// Timer rows past their due time, per tenant and kind.
struct OverdueTimerGauges {
    rows: Gauge<u64>,
    oldest_seconds: Gauge<u64>,
}

fn gauges() -> &'static OverdueTimerGauges {
    static GAUGES: OnceLock<OverdueTimerGauges> = OnceLock::new();
    GAUGES.get_or_init(|| {
        let meter = global::meter("temper.runtime");
        OverdueTimerGauges {
            rows: meter
                .u64_gauge("temper_timer_rows_overdue")
                .with_description(
                    "Timer rows past their due time, read from the store: timers that should \
                     have fired and have not, whether or not a process armed them.",
                )
                .build(),
            oldest_seconds: meter
                .u64_gauge("temper_timer_rows_overdue_oldest_seconds")
                .with_unit("s")
                .with_description(
                    "How long the earliest timer row past its due time has been due; 0 when none is.",
                )
                .build(),
        }
    })
}

/// Report a tenant's timer rows of one `kind` (`state_timeout` or
/// `scheduled_action`) that are past due, and how late the oldest is.
pub fn record_overdue_timer_rows(tenant: &str, kind: &str, rows: u64, oldest_seconds: u64) {
    let attributes = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("kind", kind.to_string()),
    ];
    let gauges = gauges();
    gauges.rows.record(rows, &attributes);
    gauges.oldest_seconds.record(oldest_seconds, &attributes);
}

use std::time::Duration;

/// Defines how an actor (or its supervisor) handles failures.
#[derive(Debug, Clone)]
pub enum SupervisionStrategy {
    /// Do not restart. Let the actor die.
    Stop,

    /// Restart the failed actor up to `max_retries` times
    /// with exponential backoff starting at `backoff_base`.
    Restart {
        max_retries: u32,
        backoff_base: Duration,
    },
}

impl SupervisionStrategy {
    /// Calculate backoff duration for the nth restart.
    /// Uses exponential backoff: base * 2^(n-1), capped at 30 seconds.
    pub fn backoff_duration(&self, restart_count: u32) -> Duration {
        match self {
            SupervisionStrategy::Stop => Duration::ZERO,
            SupervisionStrategy::Restart { backoff_base, .. } => {
                let multiplier = 2u64.saturating_pow(restart_count.saturating_sub(1));
                let backoff = backoff_base.saturating_mul(multiplier as u32);
                // Cap at 30 seconds
                std::cmp::min(backoff, Duration::from_secs(30))
            }
        }
    }
}

impl Default for SupervisionStrategy {
    fn default() -> Self {
        SupervisionStrategy::Restart {
            max_retries: 3,
            backoff_base: Duration::from_millis(100),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_exponential() {
        let strategy = SupervisionStrategy::Restart {
            max_retries: 5,
            backoff_base: Duration::from_millis(100),
        };

        assert_eq!(strategy.backoff_duration(1), Duration::from_millis(100));
        assert_eq!(strategy.backoff_duration(2), Duration::from_millis(200));
        assert_eq!(strategy.backoff_duration(3), Duration::from_millis(400));
        assert_eq!(strategy.backoff_duration(4), Duration::from_millis(800));
    }

    #[test]
    fn test_backoff_capped_at_30s() {
        let strategy = SupervisionStrategy::Restart {
            max_retries: 100,
            backoff_base: Duration::from_secs(10),
        };

        assert_eq!(strategy.backoff_duration(10), Duration::from_secs(30));
    }

    #[test]
    fn test_stop_strategy_zero_backoff() {
        let strategy = SupervisionStrategy::Stop;
        assert_eq!(strategy.backoff_duration(1), Duration::ZERO);
    }
}

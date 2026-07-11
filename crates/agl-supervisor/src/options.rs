use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{Result, SupervisorError};

pub trait SupervisorClock: Send + Sync + 'static {
    fn now_ms(&self) -> i64;
}

#[derive(Clone, Default)]
pub struct SystemSupervisorClock;

impl SupervisorClock for SystemSupervisorClock {
    fn now_ms(&self) -> i64 {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(i64::MAX)
    }
}

#[derive(Clone)]
pub struct SupervisorOptions {
    pub owner_id: String,
    pub worker_limit: usize,
    pub command_capacity: usize,
    pub subscriber_capacity: usize,
    pub lease_duration: Duration,
    pub heartbeat_interval: Duration,
    pub retry_limit: u32,
    pub retry_base_delay: Duration,
    pub retry_max_delay: Duration,
    pub clock: Arc<dyn SupervisorClock>,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            owner_id: format!(
                "supervisor-{}-{}",
                std::process::id(),
                SystemSupervisorClock.now_ms()
            ),
            worker_limit: 4,
            command_capacity: 128,
            subscriber_capacity: 128,
            lease_duration: Duration::from_secs(30),
            heartbeat_interval: Duration::from_secs(10),
            retry_limit: 3,
            retry_base_delay: Duration::from_millis(100),
            retry_max_delay: Duration::from_secs(5),
            clock: Arc::new(SystemSupervisorClock),
        }
    }
}

impl SupervisorOptions {
    pub fn validate(&self) -> Result<()> {
        if self.owner_id.trim().is_empty() {
            return Err(SupervisorError::InvalidOptions(
                "owner_id cannot be blank".to_string(),
            ));
        }
        if self.worker_limit == 0 || self.command_capacity == 0 || self.subscriber_capacity == 0 {
            return Err(SupervisorError::InvalidOptions(
                "worker and queue capacities must be positive".to_string(),
            ));
        }
        if self.heartbeat_interval.is_zero()
            || self.lease_duration.is_zero()
            || self.heartbeat_interval >= self.lease_duration
        {
            return Err(SupervisorError::InvalidOptions(
                "heartbeat_interval must be positive and shorter than lease_duration".to_string(),
            ));
        }
        if self.retry_limit == 0
            || self.retry_base_delay.is_zero()
            || self.retry_max_delay < self.retry_base_delay
        {
            return Err(SupervisorError::InvalidOptions(
                "retry policy must have positive attempts and ordered delays".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn lease_duration_ms(&self) -> i64 {
        duration_ms(self.lease_duration)
    }

    pub(crate) fn retry_delay_ms(&self, attempt: u32) -> i64 {
        let exponent = attempt.saturating_sub(1).min(31);
        let factor = 1_u128 << exponent;
        let base = self.retry_base_delay.as_millis();
        let capped = base
            .saturating_mul(factor)
            .min(self.retry_max_delay.as_millis());
        i64::try_from(capped).unwrap_or(i64::MAX)
    }
}

fn duration_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

//! Host clock for active-work prompt-cache maintenance.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tea_core::cache_warming::MaintenanceClock;

/// Wall-clock time with an executor timer.
///
/// `now` reads the system clock rather than a monotonic clock, because
/// monotonic clocks stop while a laptop sleeps. After suspension the warmer
/// must see the real elapsed time and skip a refresh whose cache entry has
/// probably expired instead of paying for a full-price write.
#[derive(Debug, Default)]
pub(super) struct SystemMaintenanceClock;

impl MaintenanceClock for SystemMaintenanceClock {
    fn now(&self) -> Duration {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
    }

    fn sleep_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let remaining = deadline.saturating_sub(self.now());
        Box::pin(async move {
            smol::Timer::after(remaining).await;
        })
    }
}

//! Retry policy driven by the shared error taxonomy.

use std::future::Future;
use std::time::Duration;

use silyn_core::SilynError;

/// Bounded exponential backoff for retryable operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(100),
        }
    }
}

impl RetryPolicy {
    /// Runs `op` until it succeeds, fails permanently, or the attempt budget is
    /// spent.
    ///
    /// Only [`SilynError::Transient`] is retried. Every other class returns
    /// immediately: retrying a denied or indeterminate operation is unsafe.
    pub async fn retry<F, Fut, T>(&self, mut op: F) -> Result<T, SilynError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, SilynError>>,
    {
        let attempts = self.max_attempts.max(1);
        let mut last_error = None;

        for attempt in 0..attempts {
            match op().await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if !error.is_retryable() {
                        return Err(error);
                    }
                    if attempt + 1 == attempts {
                        return Err(error);
                    }
                    tokio::time::sleep(self.delay(attempt)).await;
                    last_error = Some(error);
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| SilynError::Transient("retry budget exhausted".to_owned())))
    }

    fn delay(&self, attempt: u32) -> Duration {
        let factor = 2_u32.saturating_pow(attempt.min(10));
        self.base_delay
            .saturating_mul(factor)
            .min(Duration::from_secs(1))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn transient_is_retried_and_permanent_is_not() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
        };

        let calls = AtomicUsize::new(0);
        let result = policy
            .retry(|| async {
                let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if attempt < 3 {
                    Err(SilynError::Transient(format!("attempt {attempt}")))
                } else {
                    Ok(attempt)
                }
            })
            .await;
        assert_eq!(result.expect("third attempt succeeds"), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let permanent = AtomicUsize::new(0);
        let error = policy
            .retry(|| async {
                permanent.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(SilynError::Permanent("do not retry".to_owned()))
            })
            .await
            .expect_err("permanent errors are not retried");
        assert!(matches!(error, SilynError::Permanent(_)));
        assert_eq!(permanent.load(Ordering::SeqCst), 1);

        let denied = AtomicUsize::new(0);
        let error = policy
            .retry(|| async {
                denied.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(SilynError::PolicyDenied("denied".to_owned()))
            })
            .await
            .expect_err("denied operations are not retried");
        assert!(matches!(error, SilynError::PolicyDenied(_)));
        assert_eq!(denied.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
        };
        assert_eq!(policy.delay(0), Duration::from_millis(100));
        assert_eq!(policy.delay(1), Duration::from_millis(200));
        assert_eq!(policy.delay(2), Duration::from_millis(400));
        assert_eq!(policy.delay(3), Duration::from_millis(800));
        assert_eq!(policy.delay(4), Duration::from_secs(1));
        assert_eq!(policy.delay(20), Duration::from_secs(1));
    }
}

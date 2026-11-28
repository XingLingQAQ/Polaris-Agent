use std::time::Duration;
use tokio::time::Instant;

#[derive(Debug, thiserror::Error)]
#[error("server returned HTTP {status}{}", if matches!(*status, 401 | 403) { "; check the Agent token and access rules" } else { "" })]
pub struct Rejection {
    pub status: u16,
    pub retry_after: Option<Duration>,
}

impl Rejection {
    pub fn from_response(response: &reqwest::Response) -> Self {
        Self::new(
            response.status().as_u16(),
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok()),
        )
    }

    pub fn new(status: u16, retry_after: Option<&str>) -> Self {
        let retry_after = retry_after
            .and_then(|value| {
                value.trim().parse::<u64>().ok().or_else(|| {
                    let date = time::OffsetDateTime::parse(
                        value,
                        &time::format_description::well_known::Rfc2822,
                    )
                    .ok()?;
                    Some(
                        (date.unix_timestamp() - time::OffsetDateTime::now_utc().unix_timestamp())
                            .max(0) as u64,
                    )
                })
            })
            .map(|seconds| Duration::from_secs(seconds.min(3600)));
        Self {
            status,
            retry_after,
        }
    }
}

pub fn is_rejected(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<Rejection>()
        .is_some_and(|error| matches!(error.status, 401 | 403))
}

pub struct Retry {
    base: Duration,
    maximum: Duration,
    failures: u32,
    next: Instant,
    last_log: Option<Instant>,
}

impl Retry {
    pub fn new(base: Duration, maximum: Duration) -> Self {
        Self {
            base,
            maximum,
            failures: 0,
            next: Instant::now(),
            last_log: None,
        }
    }

    pub fn ready(&self) -> bool {
        Instant::now() >= self.next
    }
    pub fn deadline(&self) -> Instant {
        self.next
    }
    pub fn failures(&self) -> u32 {
        self.failures
    }

    pub fn success(&mut self, operation: &'static str) {
        if self.failures > 0 {
            tracing::info!(operation, failures = self.failures, "connection recovered");
        }
        self.failures = 0;
        self.last_log = None;
        self.next = Instant::now();
    }

    pub fn fail(&mut self, error: &anyhow::Error, operation: &'static str) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let mut random = [0; 4];
        let _ = getrandom::getrandom(&mut random);
        let delay = self.delay(error, u32::from_ne_bytes(random));
        self.next = Instant::now() + delay;
        if self
            .last_log
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(60))
        {
            tracing::warn!(operation, error = %describe(error), failures = self.failures,
                retry_in_secs = delay.as_secs_f64(), "request failed; retry scheduled");
            self.last_log = Some(Instant::now());
        } else {
            tracing::debug!(
                operation,
                failures = self.failures,
                retry_in_secs = delay.as_secs_f64(),
                "retry scheduled"
            );
        }
        delay
    }

    fn delay(&self, error: &anyhow::Error, entropy: u32) -> Duration {
        let exponential = self
            .base
            .saturating_mul(1u32 << self.failures.saturating_sub(1).min(16))
            .min(self.maximum);
        let mut delay = exponential.mul_f64(0.8 + 0.2 * f64::from(entropy) / f64::from(u32::MAX));
        if let Some(rejected) = error.downcast_ref::<Rejection>() {
            if (400..500).contains(&rejected.status) && !matches!(rejected.status, 408 | 425 | 429)
            {
                delay = delay.max(Duration::from_secs(300));
            }
            if let Some(retry_after) = rejected.retry_after {
                delay = delay.max(retry_after);
            }
        }
        delay
    }
}

pub fn describe(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    message
        .split_whitespace()
        .map(|word| {
            if ["http://", "https://", "ws://", "wss://", "socks5://"]
                .iter()
                .any(|scheme| word.contains(scheme))
            {
                "[url redacted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|ch| !ch.is_control())
        .take(512)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn backoff_is_bounded_jittered_and_resets_on_success() {
        let mut retry = Retry::new(Duration::from_secs(5), Duration::from_secs(60));
        let error = anyhow::anyhow!("offline");
        for ceiling in [5., 10., 20., 40., 60., 60.] {
            let delay = retry.fail(&error, "test");
            assert!(delay.as_secs_f64() >= ceiling * 0.8 && delay.as_secs_f64() <= ceiling);
            assert!(!retry.ready());
            tokio::time::advance(delay).await;
            assert!(retry.ready());
        }
        retry.success("test");
        assert_eq!(retry.failures(), 0);
        assert!(retry.fail(&error, "test") <= Duration::from_secs(5));
    }

    #[tokio::test]
    async fn rejection_and_retry_after_do_not_hot_loop() {
        let mut retry = Retry::new(Duration::from_secs(5), Duration::from_secs(60));
        for status in [400, 401, 403, 404] {
            let error = Rejection::new(status, None).into();
            assert!(retry.fail(&error, "test") >= Duration::from_secs(300));
        }
        let error = Rejection::new(429, Some("120")).into();
        assert_eq!(retry.fail(&error, "test"), Duration::from_secs(120));
        assert_eq!(
            Rejection::new(503, Some("999999")).retry_after,
            Some(Duration::from_secs(3600))
        );
        assert_eq!(
            Rejection::new(503, Some("Wed, 21 Oct 2015 07:28:00 GMT")).retry_after,
            Some(Duration::ZERO)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retries_remain_scheduled_after_thousands_of_failures() {
        let mut retry = Retry::new(Duration::from_secs(5), Duration::from_secs(60));
        for _ in 0..10_000 {
            let delay = retry.fail(&anyhow::anyhow!("unreachable"), "test");
            assert!(delay <= Duration::from_secs(60));
            tokio::time::advance(delay).await;
            assert!(retry.ready());
        }
        retry.success("test");
        assert_eq!(retry.failures(), 0);
    }

    #[test]
    fn logs_strip_urls_credentials_controls_and_bound_length() {
        let error = anyhow::anyhow!(
            "TLS failed for https://user:secret@host/agent/token?token=hidden\nrequest"
        );
        let message = describe(&error);
        assert_eq!(message, "TLS failed for [url redacted] request");
        assert!(describe(&anyhow::anyhow!("a".repeat(900))).len() <= 512);
    }
}

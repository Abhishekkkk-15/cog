use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{ChatRequest, ChatResponse, Provider, ProviderError, StreamEvent};

const DEFAULT_MAX_RETRIES: usize = 3;
const DEFAULT_BASE_DELAY: Duration = Duration::from_millis(500);

/// Wraps any `Provider` with exponential-backoff retry on transient errors
/// (429/5xx, timeouts, connection failures) — nothing in the provider layer
/// retried at all before this, so a single rate-limit blip killed the task.
/// Auth/validation errors (4xx other than 429) and parse/stream errors pass
/// through immediately; those aren't going to fix themselves on retry.
pub struct RetryingProvider {
    inner: Box<dyn Provider>,
    max_retries: usize,
    base_delay: Duration,
}

impl RetryingProvider {
    pub fn new(inner: Box<dyn Provider>) -> Self {
        Self { inner, max_retries: DEFAULT_MAX_RETRIES, base_delay: DEFAULT_BASE_DELAY }
    }

    #[cfg(test)]
    fn with_settings(inner: Box<dyn Provider>, max_retries: usize, base_delay: Duration) -> Self {
        Self { inner, max_retries, base_delay }
    }

    fn backoff_delay(&self, attempt: usize) -> Duration {
        self.base_delay * 2u32.pow(attempt as u32)
    }
}

fn is_retryable(err: &ProviderError) -> bool {
    match err {
        ProviderError::Api { status, .. } => matches!(status, 429 | 500 | 502 | 503 | 504),
        ProviderError::Http(e) => e.is_timeout() || e.is_connect() || e.status().map(|s| matches!(s.as_u16(), 429 | 500 | 502 | 503 | 504)).unwrap_or(false),
        ProviderError::Parse(_) | ProviderError::Stream(_) => false,
    }
}

#[async_trait]
impl Provider for RetryingProvider {
    async fn chat(&self, req: &ChatRequest) -> Result<ChatResponse, ProviderError> {
        let mut attempt = 0;
        loop {
            match self.inner.chat(req).await {
                Ok(resp) => return Ok(resp),
                Err(e) if attempt < self.max_retries && is_retryable(&e) => {
                    let delay = self.backoff_delay(attempt);
                    tracing::warn!("provider call failed ({e}), retrying in {delay:?} (attempt {}/{})", attempt + 1, self.max_retries);
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Buffers each attempt's events instead of forwarding them live: once
    /// any `StreamEvent` has reached the real caller, there's no clean way
    /// to "un-send" it on a retry, so we only know an attempt is safe to
    /// show once it's fully resolved. Costs incremental rendering on a
    /// retried call (a pause then a burst, instead of token-by-token) —
    /// acceptable since retries should be rare.
    async fn chat_stream(&self, req: &ChatRequest, tx: mpsc::UnboundedSender<StreamEvent>) -> Result<(), ProviderError> {
        let mut attempt = 0;
        loop {
            let (buf_tx, mut buf_rx) = mpsc::unbounded_channel();
            let result = self.inner.chat_stream(req, buf_tx).await;

            let mut buffered = Vec::new();
            while let Ok(evt) = buf_rx.try_recv() {
                buffered.push(evt);
            }

            match result {
                Ok(()) => {
                    for evt in buffered {
                        let _ = tx.send(evt);
                    }
                    return Ok(());
                }
                Err(e) if attempt < self.max_retries && is_retryable(&e) => {
                    let delay = self.backoff_delay(attempt);
                    tracing::warn!("provider stream failed ({e}), retrying in {delay:?} (attempt {}/{})", attempt + 1, self.max_retries);
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(e) => {
                    for evt in buffered {
                        let _ = tx.send(evt);
                    }
                    return Err(e);
                }
            }
        }
    }

    fn name(&self) -> &str {
        self.inner.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::message::{Message, Role};
    use crate::provider::FinishReason;

    /// Fails with a retryable error the first `fail_times` calls, then
    /// succeeds — lets tests assert the retry loop actually retries.
    struct FlakyProvider {
        fail_times: usize,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for FlakyProvider {
        async fn chat(&self, _req: &ChatRequest) -> Result<ChatResponse, ProviderError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                return Err(ProviderError::Api { status: 503, message: "service unavailable".into() });
            }
            Ok(ChatResponse {
                message: Message { role: Role::Assistant, content: Some("ok".into()), ..Default::default() },
                finish_reason: FinishReason::Stop,
                usage: None,
            })
        }

        fn name(&self) -> &str {
            "flaky"
        }
    }

    struct AlwaysAuthError;

    #[async_trait]
    impl Provider for AlwaysAuthError {
        async fn chat(&self, _req: &ChatRequest) -> Result<ChatResponse, ProviderError> {
            Err(ProviderError::Api { status: 401, message: "invalid api key".into() })
        }

        fn name(&self) -> &str {
            "always-401"
        }
    }

    fn dummy_request() -> ChatRequest {
        ChatRequest { model: "test".into(), messages: vec![], tools: None, tool_choice: None, stream: false, temperature: None, max_tokens: None }
    }

    #[tokio::test]
    async fn retries_on_retryable_errors_until_success() {
        let provider = RetryingProvider::with_settings(Box::new(FlakyProvider { fail_times: 2, calls: AtomicUsize::new(0) }), 3, Duration::from_millis(1));
        let result = provider.chat(&dummy_request()).await;
        assert!(result.is_ok(), "should eventually succeed after retrying past the flaky failures");
    }

    #[tokio::test]
    async fn gives_up_after_max_retries_exhausted() {
        let provider = RetryingProvider::with_settings(Box::new(FlakyProvider { fail_times: 100, calls: AtomicUsize::new(0) }), 2, Duration::from_millis(1));
        let result = provider.chat(&dummy_request()).await;
        assert!(result.is_err(), "should give up once retries are exhausted");
    }

    #[tokio::test]
    async fn does_not_retry_non_retryable_errors() {
        let provider = RetryingProvider::with_settings(Box::new(AlwaysAuthError), 3, Duration::from_millis(1));
        let result = provider.chat(&dummy_request()).await;
        assert!(result.is_err(), "a 401 should pass straight through, not retry");
    }

    #[test]
    fn classifies_retryable_status_codes() {
        assert!(is_retryable(&ProviderError::Api { status: 429, message: String::new() }));
        assert!(is_retryable(&ProviderError::Api { status: 503, message: String::new() }));
        assert!(!is_retryable(&ProviderError::Api { status: 401, message: String::new() }));
        assert!(!is_retryable(&ProviderError::Api { status: 400, message: String::new() }));
        assert!(!is_retryable(&ProviderError::Parse("bad json".into())));
    }
}

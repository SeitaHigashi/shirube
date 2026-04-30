use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::debug;

/// トークンバケット方式のレートリミッター。
///
/// bitFlyer APIは200 req/minの制限があるため、
/// デフォルトは190 req/min（安全マージン）。
pub struct RateLimiter {
    /// バケット容量（最大トークン数）
    capacity: u32,
    /// 現在のトークン数
    tokens: Mutex<f64>,
    /// トークン補充レート (tokens/sec)
    refill_rate: f64,
    /// 最終補充時刻
    last_refill: Mutex<Instant>,
}

impl RateLimiter {
    /// `max_per_minute` req/min のレートリミッターを作成する。
    pub fn new(max_per_minute: u32) -> Self {
        let capacity = max_per_minute;
        let refill_rate = max_per_minute as f64 / 60.0;
        Self {
            capacity,
            tokens: Mutex::new(capacity as f64),
            refill_rate,
            last_refill: Mutex::new(Instant::now()),
        }
    }

    /// トークンを1つ取得する。必要なら待機する。
    pub async fn acquire(&self) {
        loop {
            let wait = self.try_acquire();
            if let Some(wait_duration) = wait {
                debug!("Rate limiter: waiting {:?}", wait_duration);
                tokio::time::sleep(wait_duration).await;
            } else {
                return;
            }
        }
    }

    /// トークンを取得を試みる。
    /// - `None`: 取得成功
    /// - `Some(duration)`: 待機が必要な時間
    fn try_acquire(&self) -> Option<Duration> {
        let mut tokens = self.tokens.lock().unwrap();
        let mut last_refill = self.last_refill.lock().unwrap();

        // 経過時間に応じてトークンを補充
        let now = Instant::now();
        let elapsed = now.duration_since(*last_refill).as_secs_f64();
        *tokens = (*tokens + elapsed * self.refill_rate).min(self.capacity as f64);
        *last_refill = now;

        if *tokens >= 1.0 {
            *tokens -= 1.0;
            None
        } else {
            // 不足分を補充するのに必要な時間
            let deficit = 1.0 - *tokens;
            let wait_secs = deficit / self.refill_rate;
            Some(Duration::from_secs_f64(wait_secs))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquire_succeeds_within_capacity() {
        let limiter = RateLimiter::new(60);
        // 初期トークンが満タンなのですぐに取得できる
        for _ in 0..10 {
            tokio::time::timeout(
                Duration::from_millis(10),
                limiter.acquire(),
            )
            .await
            .expect("should not wait with full bucket");
        }
    }

    #[test]
    fn try_acquire_depletes_tokens() {
        let limiter = RateLimiter::new(5);
        // 5トークンを全て取得
        for _ in 0..5 {
            assert!(limiter.try_acquire().is_none(), "should succeed");
        }
        // 6番目は待機が必要
        assert!(limiter.try_acquire().is_some(), "should need to wait");
    }

    #[test]
    fn rate_limiter_refill_over_time() {
        let limiter = RateLimiter::new(60); // 1 token/sec
        // バケットを空にする
        {
            let mut tokens = limiter.tokens.lock().unwrap();
            *tokens = 0.0;
            let mut last = limiter.last_refill.lock().unwrap();
            *last = Instant::now() - Duration::from_secs(2); // 2秒前に設定
        }
        // 2秒分 = 2トークン補充されているはず
        assert!(limiter.try_acquire().is_none(), "should have refilled");
        assert!(limiter.try_acquire().is_none(), "should have 2nd token");
        assert!(limiter.try_acquire().is_some(), "3rd should need wait");
    }

    #[test]
    fn capacity_is_not_exceeded() {
        let limiter = RateLimiter::new(5);
        {
            let mut tokens = limiter.tokens.lock().unwrap();
            *tokens = 0.0;
            let mut last = limiter.last_refill.lock().unwrap();
            // 100秒前に設定しても capacity=5 を超えない
            *last = Instant::now() - Duration::from_secs(100);
        }
        // tryを呼んでトークンを補充
        let _ = limiter.try_acquire();
        let tokens = *limiter.tokens.lock().unwrap();
        // 補充後は capacity-1 = 4 になるはず
        assert!(tokens <= 4.01, "tokens should not exceed capacity-1 after acquire, got {}", tokens);
    }
}

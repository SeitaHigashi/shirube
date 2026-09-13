use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::sync_ext::MutexExt;

use tracing::debug;

/// トークンバケット方式のレートリミッター。
///
/// bitFlyer の上限は 200 req/min（公開・認証共通）で、`new(200)` がその上限
/// ちょうどを表す。安全マージンを取りたい呼び出し側は小さい値を渡すこと。
///
/// NOTE: このコメントは以前「デフォルトは190 req/min（安全マージン）」と
/// 書いていたが、実際の唯一の呼び出し箇所である `BitFlyerRestClient` は
/// `new(200)` を渡しており、190 というマージンはコード上どこにも存在
/// しなかった。誤解を避けるため実装に合わせて記述を修正した。
///
/// # 観測用カウンタ
///
/// `granted` / `waited_micros` は「この予算がどれだけ逼迫しているか」を
/// 呼び出し側から読めるようにするためのもので、取得判定には一切関与しない。
/// ペーパートレードで本番と同じ逼迫が再現できているかを確認する用途を
/// 想定している（`MockExchangeClient` はリミッタを持たないため、これが
/// なければ枯渇は不可視のままになる）。
pub struct RateLimiter {
    /// バケット容量（最大トークン数）
    capacity: u32,
    /// 現在のトークン数
    tokens: Mutex<f64>,
    /// トークン補充レート (tokens/sec)
    refill_rate: f64,
    /// 最終補充時刻
    last_refill: Mutex<Instant>,
    /// 取得できたトークンの累計数（= 通過したリクエスト数）。観測専用。
    granted: AtomicU64,
    /// 補充待ちに費やした累計時間（マイクロ秒）。観測専用。
    waited_micros: AtomicU64,
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
            granted: AtomicU64::new(0),
            waited_micros: AtomicU64::new(0),
        }
    }

    /// これまでに通過したリクエスト数。観測専用。
    pub fn granted(&self) -> u64 {
        self.granted.load(Ordering::Relaxed)
    }

    /// これまでに補充待ちで費やした合計時間。観測専用。
    ///
    /// これが経過時間に対して無視できない割合になっている場合、その
    /// クライアントはレート予算を飽和させており、発注もその待ち行列の
    /// 後ろに並んでいることを意味する。
    pub fn waited(&self) -> Duration {
        Duration::from_micros(self.waited_micros.load(Ordering::Relaxed))
    }

    /// 現在のトークン残量（補充を反映しない瞬間値）。観測専用。
    pub fn available_tokens(&self) -> f64 {
        *self.tokens.lock_or_recover()
    }

    /// トークンを1つ取得する。必要なら待機する。
    pub async fn acquire(&self) {
        let mut waited = Duration::ZERO;
        loop {
            let wait = self.try_acquire();
            if let Some(wait_duration) = wait {
                debug!("Rate limiter: waiting {:?}", wait_duration);
                tokio::time::sleep(wait_duration).await;
                waited += wait_duration;
            } else {
                self.granted.fetch_add(1, Ordering::Relaxed);
                if !waited.is_zero() {
                    self.waited_micros
                        .fetch_add(waited.as_micros() as u64, Ordering::Relaxed);
                }
                return;
            }
        }
    }

    /// トークンを取得を試みる。
    /// - `None`: 取得成功
    /// - `Some(duration)`: 待機が必要な時間
    fn try_acquire(&self) -> Option<Duration> {
        let mut tokens = self.tokens.lock_or_recover();
        let mut last_refill = self.last_refill.lock_or_recover();

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

use reqwest::Client;
use tracing::{error, warn};

/// アラートの重要度
#[derive(Debug, Clone, PartialEq)]
pub enum AlertLevel {
    Info,
    Warn,
    Critical,
}

impl AlertLevel {
    fn emoji(&self) -> &'static str {
        match self {
            AlertLevel::Info => ":information_source:",
            AlertLevel::Warn => ":warning:",
            AlertLevel::Critical => ":rotating_light:",
        }
    }
}

/// ログ出力 + オプションで Slack Webhook にアラートを送信する。
pub struct AlertManager {
    slack_webhook_url: Option<String>,
    client: Client,
}

impl AlertManager {
    pub fn new() -> Self {
        let slack_webhook_url = std::env::var("SLACK_WEBHOOK_URL").ok();
        if slack_webhook_url.is_some() {
            tracing::info!("AlertManager: Slack webhook configured");
        } else {
            tracing::info!("AlertManager: Slack webhook not configured (log-only mode)");
        }
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("Failed to build HTTP client");
        Self { slack_webhook_url, client }
    }

    /// アラートを送信する。ログは必ず出力し、Slack設定済みなら通知する。
    pub async fn send(&self, level: AlertLevel, message: &str) {
        // ログ出力
        match level {
            AlertLevel::Info => tracing::info!(alert = message, "Alert"),
            AlertLevel::Warn => warn!(alert = message, "Alert"),
            AlertLevel::Critical => error!(alert = message, "Alert [CRITICAL]"),
        }

        // Slack Webhook 送信
        if let Some(url) = &self.slack_webhook_url {
            let text = format!("{} *trader2* {}", level.emoji(), message);
            let payload = serde_json::json!({ "text": text });
            if let Err(e) = self.client.post(url).json(&payload).send().await {
                warn!("Failed to send Slack alert: {}", e);
            }
        }
    }

    /// サーキットブレーカー発動アラート
    pub async fn circuit_breaker_triggered(&self, drawdown_pct: f64) {
        self.send(
            AlertLevel::Critical,
            &format!(
                "Circuit breaker triggered! Daily drawdown: {:.2}%",
                drawdown_pct * 100.0
            ),
        )
        .await;
    }

    /// WS切断アラート
    pub async fn ws_disconnected(&self, reason: &str) {
        self.send(AlertLevel::Warn, &format!("WebSocket disconnected: {}", reason)).await;
    }

    /// 注文エラーアラート
    pub async fn order_error(&self, reason: &str) {
        self.send(AlertLevel::Critical, &format!("Order error: {}", reason)).await;
    }
}

impl Default for AlertManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_does_not_panic_without_slack() {
        std::env::remove_var("SLACK_WEBHOOK_URL");
        let manager = AlertManager::new();
        // Slack なしでもパニックしない
        manager.send(AlertLevel::Info, "test info").await;
        manager.send(AlertLevel::Warn, "test warn").await;
        manager.send(AlertLevel::Critical, "test critical").await;
    }

    #[tokio::test]
    async fn circuit_breaker_alert_formats_correctly() {
        std::env::remove_var("SLACK_WEBHOOK_URL");
        let manager = AlertManager::new();
        // パニックしないことを確認
        manager.circuit_breaker_triggered(0.06).await;
    }

    #[tokio::test]
    async fn ws_disconnected_alert() {
        std::env::remove_var("SLACK_WEBHOOK_URL");
        let manager = AlertManager::new();
        manager.ws_disconnected("connection reset").await;
    }

    #[test]
    fn alert_level_emoji() {
        assert_eq!(AlertLevel::Info.emoji(), ":information_source:");
        assert_eq!(AlertLevel::Warn.emoji(), ":warning:");
        assert_eq!(AlertLevel::Critical.emoji(), ":rotating_light:");
    }

    #[test]
    fn alert_manager_no_slack_when_env_not_set() {
        std::env::remove_var("SLACK_WEBHOOK_URL");
        let manager = AlertManager::new();
        assert!(manager.slack_webhook_url.is_none());
    }
}

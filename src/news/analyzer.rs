use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentimentScore {
    pub headline: String,
    /// センチメントスコア: -1.0（強い売り）〜 +1.0（強い買い）
    pub score: f64,
    pub analyzed_at: DateTime<Utc>,
}

/// Ollamaへのリクエスト本文
#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
}

/// Ollamaからのレスポンス
#[derive(Deserialize)]
struct OllamaResponse {
    response: String,
}

/// Ollama HTTP APIでニュースのセンチメントを判定する。
pub struct NewsAnalyzer {
    ollama_url: String,
    model: String,
    client: Client,
}

impl NewsAnalyzer {
    pub fn new(ollama_url: impl Into<String>, model: impl Into<String>) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("Failed to build HTTP client");
        Self { ollama_url: ollama_url.into(), model: model.into(), client }
    }

    /// ヘッドラインのセンチメントスコアを返す。
    /// Ollamaが不在の場合は 0.0 にフォールバックする。
    pub async fn analyze(&self, headline: &str) -> f64 {
        match self.call_ollama(headline).await {
            Ok(score) => score,
            Err(e) => {
                warn!("Ollama unavailable ({}), falling back to score=0.0", e);
                0.0
            }
        }
    }

    async fn call_ollama(&self, headline: &str) -> anyhow::Result<f64> {
        let prompt = format!(
            "Analyze the sentiment of this crypto news headline. \
             Respond with ONLY one of: BULLISH, BEARISH, or NEUTRAL.\n\
             Headline: {}",
            headline
        );

        let url = format!("{}/api/generate", self.ollama_url);
        let req = OllamaRequest { model: &self.model, prompt: &prompt, stream: false };
        let resp = self
            .client
            .post(&url)
            .json(&req)
            .send()
            .await?
            .json::<OllamaResponse>()
            .await?;

        let score = parse_sentiment(&resp.response);
        debug!(headline = headline, score = score, "Ollama sentiment");
        Ok(score)
    }

    /// 複数ヘッドラインをバッチ分析してスコアリストを返す。
    pub async fn analyze_batch(&self, items: &[crate::news::fetcher::NewsItem]) -> Vec<SentimentScore> {
        let mut scores = Vec::with_capacity(items.len());
        for item in items {
            let score = self.analyze(&item.headline).await;
            scores.push(SentimentScore {
                headline: item.headline.clone(),
                score,
                analyzed_at: Utc::now(),
            });
        }
        scores
    }
}

/// Ollamaのテキストレスポンスからスコアを抽出する。
fn parse_sentiment(text: &str) -> f64 {
    let upper = text.to_uppercase();
    if upper.contains("BULLISH") {
        1.0
    } else if upper.contains("BEARISH") {
        -1.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bullish() {
        assert_eq!(parse_sentiment("BULLISH"), 1.0);
        assert_eq!(parse_sentiment("The market is BULLISH right now"), 1.0);
    }

    #[test]
    fn parse_bearish() {
        assert_eq!(parse_sentiment("BEARISH"), -1.0);
        assert_eq!(parse_sentiment("Sentiment is BEARISH"), -1.0);
    }

    #[test]
    fn parse_neutral() {
        assert_eq!(parse_sentiment("NEUTRAL"), 0.0);
        assert_eq!(parse_sentiment("I don't know"), 0.0);
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!(parse_sentiment("bullish"), 1.0);
        assert_eq!(parse_sentiment("bearish"), -1.0);
    }

    #[test]
    fn sentiment_score_is_serializable() {
        let score = SentimentScore {
            headline: "BTC hits ATH".into(),
            score: 1.0,
            analyzed_at: Utc::now(),
        };
        let json = serde_json::to_string(&score).unwrap();
        assert!(json.contains("BTC hits ATH"));
        assert!(json.contains("1.0"));
    }

    #[tokio::test]
    async fn analyze_falls_back_when_ollama_absent() {
        let analyzer = NewsAnalyzer::new("http://localhost:11999", "llama3");
        let score = analyzer.analyze("Bitcoin jumps 10%").await;
        assert_eq!(score, 0.0);
    }
}

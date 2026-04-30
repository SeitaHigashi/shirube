use rust_decimal::Decimal;

use crate::news::analyzer::SentimentScore;
use crate::signal::Signal;

const NEWS_WEIGHT: f64 = 0.3;
const TA_WEIGHT: f64 = 0.7;
const SENTIMENT_THRESHOLD: f64 = 0.3;

/// TAシグナルとニュースセンチメントを合成するスコアラー。
pub struct NewsScorer {
    news_weight: f64,
    ta_weight: f64,
}

impl Default for NewsScorer {
    fn default() -> Self {
        Self { news_weight: NEWS_WEIGHT, ta_weight: TA_WEIGHT }
    }
}

impl NewsScorer {
    pub fn new(news_weight: f64) -> Self {
        let ta_weight = 1.0 - news_weight;
        Self { news_weight, ta_weight }
    }

    /// TAの信頼度とセンチメントスコアを加重平均で合成する。
    pub fn combined_confidence(&self, ta_confidence: f64, sentiment: f64) -> f64 {
        ta_confidence * self.ta_weight + sentiment.abs() * self.news_weight
    }

    /// センチメントスコアから Signal を生成する。
    pub fn sentiment_to_signal(&self, sentiment: f64, price: Decimal) -> Signal {
        if sentiment >= SENTIMENT_THRESHOLD {
            Signal::Buy { price, confidence: sentiment }
        } else if sentiment <= -SENTIMENT_THRESHOLD {
            Signal::Sell { price, confidence: sentiment.abs() }
        } else {
            Signal::Hold
        }
    }

    /// スコアリストの平均センチメントスコアを返す。
    pub fn average_score(&self, scores: &[SentimentScore]) -> f64 {
        if scores.is_empty() {
            return 0.0;
        }
        let sum: f64 = scores.iter().map(|s| s.score).sum();
        sum / scores.len() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn score(s: f64) -> SentimentScore {
        SentimentScore { headline: "test".into(), score: s, analyzed_at: Utc::now(), published_at: None }
    }

    #[test]
    fn combined_confidence_weighted_average() {
        let scorer = NewsScorer::default();
        let result = scorer.combined_confidence(1.0, 1.0);
        assert!((result - 1.0).abs() < 1e-9);
    }

    #[test]
    fn combined_confidence_ta_dominant() {
        let scorer = NewsScorer::default();
        // ta=0.8, sentiment=0.0 → 0.8 * 0.7 + 0.0 * 0.3 = 0.56
        let result = scorer.combined_confidence(0.8, 0.0);
        assert!((result - 0.56).abs() < 1e-9);
    }

    #[test]
    fn combined_confidence_uses_abs_of_sentiment() {
        let scorer = NewsScorer::default();
        let pos = scorer.combined_confidence(0.5, 0.6);
        let neg = scorer.combined_confidence(0.5, -0.6);
        assert!((pos - neg).abs() < 1e-9);
    }

    #[test]
    fn sentiment_to_signal_bullish() {
        let scorer = NewsScorer::default();
        let sig = scorer.sentiment_to_signal(0.8, dec!(9000000));
        assert!(matches!(sig, Signal::Buy { .. }));
    }

    #[test]
    fn sentiment_to_signal_bearish() {
        let scorer = NewsScorer::default();
        let sig = scorer.sentiment_to_signal(-0.5, dec!(9000000));
        assert!(matches!(sig, Signal::Sell { .. }));
    }

    #[test]
    fn sentiment_to_signal_neutral_hold() {
        let scorer = NewsScorer::default();
        let sig = scorer.sentiment_to_signal(0.1, dec!(9000000));
        assert_eq!(sig, Signal::Hold);
    }

    #[test]
    fn average_score_empty() {
        let scorer = NewsScorer::default();
        assert_eq!(scorer.average_score(&[]), 0.0);
    }

    #[test]
    fn average_score_mixed() {
        let scorer = NewsScorer::default();
        let scores = vec![score(1.0), score(-1.0), score(0.0)];
        assert!((scorer.average_score(&scores)).abs() < 1e-9);
    }

    #[test]
    fn average_score_all_bullish() {
        let scorer = NewsScorer::default();
        let scores = vec![score(1.0), score(1.0)];
        assert!((scorer.average_score(&scores) - 1.0).abs() < 1e-9);
    }
}

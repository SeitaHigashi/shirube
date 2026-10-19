use crate::news::analyzer::SentimentScore;

/// TAシグナルとニュースセンチメントを合成するスコアラー。
pub struct NewsScorer;

impl Default for NewsScorer {
    fn default() -> Self {
        Self
    }
}

impl NewsScorer {
    pub fn new() -> Self {
        Self
    }

    /// スコアリストの平均センチメントスコアを返す。
    /// 空リストの場合は 0.0 を返す。
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

    fn score(s: f64) -> SentimentScore {
        SentimentScore { headline: "test".into(), score: s, analyzed_at: Utc::now(), published_at: None }
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

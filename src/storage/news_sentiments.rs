use chrono::{DateTime, Utc};
use tokio_rusqlite::Connection;

use crate::error::Result;
use crate::news::analyzer::SentimentScore;
use crate::news::fetcher::NewsItem;

pub struct NewsSentimentRepository {
    conn: Connection,
}

impl NewsSentimentRepository {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// 解析結果を一括保存する。同一 (headline, analyzed_at) は無視する。
    pub async fn insert_batch(&self, items: &[NewsItem], scores: &[SentimentScore]) -> Result<()> {
        // headline をキーに NewsItem を引けるようにする
        let item_map: std::collections::HashMap<&str, &NewsItem> =
            items.iter().map(|i| (i.headline.as_str(), i)).collect();

        let rows: Vec<(String, String, Option<String>, f64, String)> = scores
            .iter()
            .map(|s| {
                let item = item_map.get(s.headline.as_str());
                let url = item.map(|i| i.url.clone()).unwrap_or_default();
                let body = item.and_then(|i| i.body.clone());
                (
                    s.headline.clone(),
                    url,
                    body,
                    s.score,
                    s.analyzed_at.to_rfc3339(),
                )
            })
            .collect();

        self.conn
            .call(move |c| {
                let tx = c.transaction()?;
                {
                    let mut stmt = tx.prepare(
                        "INSERT OR IGNORE INTO news_sentiments
                             (headline, url, body, score, analyzed_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                    )?;
                    for (headline, url, body, score, analyzed_at) in &rows {
                        stmt.execute(rusqlite::params![
                            headline, url, body, score, analyzed_at
                        ])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// 新しい順に最大 `limit` 件取得する。
    pub async fn get_latest(&self, limit: u32) -> Result<Vec<SentimentScore>> {
        let rows = self
            .conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT headline, score, analyzed_at
                     FROM news_sentiments
                     ORDER BY analyzed_at DESC
                     LIMIT ?1",
                )?;
                let rows = stmt.query_map(rusqlite::params![limit], |row| {
                    let headline: String = row.get(0)?;
                    let score: f64 = row.get(1)?;
                    let analyzed_at_str: String = row.get(2)?;
                    Ok((headline, score, analyzed_at_str))
                })?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;

        let scores = rows
            .into_iter()
            .map(|(headline, score, analyzed_at_str)| {
                let analyzed_at = DateTime::parse_from_rfc3339(&analyzed_at_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                SentimentScore { headline, score, analyzed_at }
            })
            .collect();
        Ok(scores)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::Database;

    fn make_item(headline: &str, url: &str, body: Option<&str>) -> NewsItem {
        NewsItem {
            headline: headline.into(),
            url: url.into(),
            published_at: Utc::now(),
            body: body.map(|s| s.into()),
        }
    }

    fn make_score(headline: &str, score: f64) -> SentimentScore {
        SentimentScore {
            headline: headline.into(),
            score,
            analyzed_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn insert_and_get_latest() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.news_sentiments();

        let items = vec![
            make_item("BTC hits new ATH", "https://example.com/1", Some("Bitcoin surged...")),
            make_item("Crypto market crashes", "https://example.com/2", None),
        ];
        let scores = vec![make_score("BTC hits new ATH", 1.0), make_score("Crypto market crashes", -1.0)];

        repo.insert_batch(&items, &scores).await.unwrap();

        let result = repo.get_latest(10).await.unwrap();
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn insert_ignore_duplicate() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.news_sentiments();

        let items = vec![make_item("BTC up", "https://example.com/1", None)];
        let scores = vec![make_score("BTC up", 1.0)];

        repo.insert_batch(&items, &scores).await.unwrap();
        repo.insert_batch(&items, &scores).await.unwrap();

        let result = repo.get_latest(10).await.unwrap();
        assert_eq!(result.len(), 1);
    }

    #[tokio::test]
    async fn get_latest_respects_limit() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.news_sentiments();

        let items: Vec<NewsItem> = (0..5)
            .map(|i| make_item(&format!("headline {}", i), "", None))
            .collect();
        let scores: Vec<SentimentScore> =
            (0..5).map(|i| make_score(&format!("headline {}", i), 0.0)).collect();

        repo.insert_batch(&items, &scores).await.unwrap();

        let result = repo.get_latest(3).await.unwrap();
        assert_eq!(result.len(), 3);
    }
}

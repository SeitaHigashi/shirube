use chrono::{DateTime, Utc};
use reqwest::Client;
use thiserror::Error;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewsItem {
    pub headline: String,
    pub url: String,
    pub published_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Feed parse error: {0}")]
    Parse(String),
}

/// RSS/Atom フィードからニュース記事を取得する。
#[async_trait::async_trait]
pub trait FeedSource: Send + Sync {
    async fn fetch_latest(&self) -> Result<Vec<NewsItem>, FetchError>;
}

pub struct NewsFetcher {
    feed_urls: Vec<String>,
    client: Client,
}

impl NewsFetcher {
    pub fn new(feed_urls: Vec<String>) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");
        Self { feed_urls, client }
    }

    async fn fetch_feed(&self, url: &str) -> Result<Vec<NewsItem>, FetchError> {
        let bytes = self.client.get(url).send().await?.bytes().await?;
        let feed = feed_rs::parser::parse(bytes.as_ref())
            .map_err(|e| FetchError::Parse(e.to_string()))?;

        let items = feed
            .entries
            .into_iter()
            .filter_map(|entry| {
                let headline = entry
                    .title
                    .map(|t| t.content)
                    .unwrap_or_default();
                if headline.is_empty() {
                    return None;
                }
                let entry_url = entry
                    .links
                    .first()
                    .map(|l| l.href.clone())
                    .unwrap_or_default();
                let published_at = entry
                    .published
                    .or(entry.updated)
                    .unwrap_or_else(Utc::now);
                Some(NewsItem { headline, url: entry_url, published_at })
            })
            .collect();

        Ok(items)
    }
}

#[async_trait::async_trait]
impl FeedSource for NewsFetcher {
    async fn fetch_latest(&self) -> Result<Vec<NewsItem>, FetchError> {
        let mut all_items: Vec<NewsItem> = Vec::new();
        for url in &self.feed_urls {
            match self.fetch_feed(url).await {
                Ok(items) => all_items.extend(items),
                Err(e) => {
                    tracing::warn!("Failed to fetch feed {}: {}", url, e);
                }
            }
        }
        // 重複URLを除去し、新しい順に並べる
        let mut seen = std::collections::HashSet::new();
        all_items.retain(|item| seen.insert(item.url.clone()));
        all_items.sort_by(|a, b| b.published_at.cmp(&a.published_at));
        Ok(all_items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn news_item_is_serializable() {
        let item = NewsItem {
            headline: "Bitcoin surges to new high".into(),
            url: "https://example.com/news/1".into(),
            published_at: Utc::now(),
        };
        let json = serde_json::to_string(&item).unwrap();
        assert!(json.contains("Bitcoin surges"));
    }

    #[test]
    fn new_fetcher_builds_with_empty_urls() {
        let fetcher = NewsFetcher::new(vec![]);
        assert!(fetcher.feed_urls.is_empty());
    }

    #[test]
    fn new_fetcher_stores_urls() {
        let urls = vec!["https://example.com/feed.xml".into()];
        let fetcher = NewsFetcher::new(urls.clone());
        assert_eq!(fetcher.feed_urls, urls);
    }
}

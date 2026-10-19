<script lang="ts">
  import { newsItems } from '../lib/stores';

  const JST_OFFSET = 9 * 3600 * 1000;

  function formatTime(isoSrc: string | null): string {
    if (!isoSrc) return '';
    const t = new Date(isoSrc);
    const utcStr = t.toISOString().slice(5, 16).replace('T', ' ');
    const jstStr = new Date(t.getTime() + JST_OFFSET).toISOString().slice(11, 16);
    return `${utcStr}Z (${jstStr} JST)`;
  }

  function badgeClass(score: number): string {
    return score > 0.1 ? 'bullish' : score < -0.1 ? 'bearish' : 'neutral';
  }

  function badgeLabel(score: number): string {
    return score > 0.1 ? '強気' : score < -0.1 ? '弱気' : '中立';
  }

  $: items = $newsItems;
  $: avg = items.length > 0 ? items.reduce((s, i) => s + i.score, 0) / items.length : null;
  $: avgClass = avg === null ? 'neutral' : avg > 0.1 ? 'up' : avg < -0.1 ? 'down' : 'neutral';
</script>

<section class="news-section">
  <h2>ニュースセンチメント (AI分析)</h2>
  <div class="news-avg-bar">
    <span class="news-avg-label">平均スコア:</span>
    <span class="news-avg-score {avgClass}">
      {avg !== null ? avg.toFixed(2) : '---'}
    </span>
  </div>
  <div class="news-list">
    {#if items.length === 0}
      <span class="empty">データなし (Ollamaが未起動の場合はスコア0で表示されます)</span>
    {:else}
      {#each items as item}
        <div class="news-item">
          <span class="news-score-badge {badgeClass(item.score)}">{badgeLabel(item.score)}</span>
          <span class="news-headline">{item.headline}</span>
          <span class="news-time">{formatTime(item.published_at || item.analyzed_at)}</span>
        </div>
      {/each}
    {/if}
  </div>
</section>

<style>
  .news-section {
    background: #16213e;
    margin: 12px;
    padding: 16px;
    border-radius: 8px;
  }
  h2 {
    font-size: 13px;
    color: #888;
    text-transform: uppercase;
    letter-spacing: 0.5px;
    margin: 0 0 12px;
  }
  .news-avg-bar {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 12px;
  }
  .news-avg-label { font-size: 12px; color: #888; }
  .news-avg-score { font-size: 22px; font-weight: bold; }
  .up { color: #26a69a; }
  .down { color: #ef5350; }
  .neutral { color: #e0e0e0; }

  .news-list {
    max-height: 300px;
    overflow-y: auto;
    scrollbar-width: thin;
    scrollbar-color: #0f3460 transparent;
  }
  .news-list::-webkit-scrollbar { width: 4px; }
  .news-list::-webkit-scrollbar-thumb { background: #0f3460; border-radius: 2px; }

  .news-item {
    padding: 8px 0;
    border-bottom: 1px solid #0f3460;
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .news-item:last-child { border-bottom: none; }

  .news-score-badge {
    min-width: 48px;
    text-align: center;
    font-weight: bold;
    font-size: 13px;
    padding: 2px 6px;
    border-radius: 4px;
  }
  .bullish { background: rgba(38,166,154,0.2); color: #26a69a; }
  .bearish { background: rgba(239,83,80,0.2); color: #ef5350; }
  .neutral-badge { background: rgba(255,255,255,0.05); color: #888; }

  .news-score-badge.neutral {
    background: rgba(255,255,255,0.05);
    color: #888;
  }

  .news-headline {
    font-size: 12px;
    color: #ccc;
    flex: 1;
  }
  .news-time {
    font-size: 10px;
    color: #555;
    white-space: nowrap;
  }
  .empty {
    color: #888;
    font-size: 12px;
  }
</style>

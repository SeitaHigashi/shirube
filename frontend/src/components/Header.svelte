<script lang="ts">
  import { ticker } from '../lib/stores';
</script>

<header>
  <h1>導</h1>
  <span class="pair">BTC/JPY</span>
  <span class="tz">UTC</span>
  <div class="ticker-info">
    <div class="stat">
      <div class="stat-label">最終価格</div>
      <div class="stat-value neutral">
        {$ticker ? Number($ticker.ltp).toLocaleString('ja-JP') : '---'}
      </div>
    </div>
    <div class="stat">
      <div class="stat-label">BID</div>
      <div class="stat-value up">
        {$ticker ? Number($ticker.best_bid).toLocaleString('ja-JP') : '---'}
      </div>
    </div>
    <div class="stat">
      <div class="stat-label">ASK</div>
      <div class="stat-value down">
        {$ticker ? Number($ticker.best_ask).toLocaleString('ja-JP') : '---'}
      </div>
    </div>
    <div class="stat">
      <div class="stat-label">出来高</div>
      <div class="stat-value neutral">
        {$ticker ? Number($ticker.volume_by_product).toFixed(2) : '---'}
      </div>
    </div>
  </div>
</header>

<style>
  header {
    background: #16213e;
    padding: 12px 20px;
    display: flex;
    align-items: center;
    gap: 16px;
    border-bottom: 1px solid #0f3460;
    flex-wrap: wrap;
  }
  h1 {
    font-size: 18px;
    color: #e94560;
    margin: 0;
  }
  .pair {
    color: #888;
    font-size: 12px;
  }
  .tz {
    color: #888;
    font-size: 11px;
  }
  .ticker-info {
    margin-left: auto;
    display: flex;
    gap: 24px;
    flex-wrap: wrap;
  }
  .stat {
    text-align: right;
  }
  .stat-label {
    color: #888;
    font-size: 11px;
  }
  .stat-value {
    font-size: 16px;
    font-weight: bold;
  }
  .up { color: #26a69a; }
  .down { color: #ef5350; }
  .neutral { color: #e0e0e0; }

  @media (max-width: 767px) {
    header {
      padding: 8px 12px;
      gap: 8px;
    }
    .ticker-info {
      width: 100%;
      margin-left: 0;
      justify-content: space-between;
      gap: 8px;
    }
    .stat {
      text-align: left;
    }
    .stat-value {
      font-size: 13px;
    }
  }
</style>

<script lang="ts">
  import { onMount } from 'svelte';
  import { fetchConfig, saveConfig, fetchIndicators } from '../lib/api';
  import type { TradingConfig } from '../lib/types';

  let cfg: TradingConfig | null = null;
  let msg = '';
  let msgOk = false;
  let saving = false;

  onMount(async () => {
    try {
      cfg = await fetchConfig();
    } catch (e) {
      showMsg('設定の読み込みに失敗しました', false);
    }
  });

  function showMsg(text: string, ok: boolean) {
    msg = text;
    msgOk = ok;
    if (ok) setTimeout(() => { msg = ''; }, 3000);
  }

  async function handleSave() {
    if (!cfg) return;
    saving = true;
    try {
      const result = await saveConfig(cfg);
      if (!result.ok) {
        showMsg('エラー: ' + (result.error || '不明'), false);
      } else {
        showMsg('保存しました', true);
      }
    } catch (e: any) {
      showMsg('通信エラー: ' + e.message, false);
    } finally {
      saving = false;
    }
  }
</script>

<section class="settings-section">
  <h2>取引設定</h2>

  {#if cfg}
    <h3>リスク管理</h3>
    <div class="settings-grid">
      <label class="settings-field">最大BTC保有量
        <input type="number" bind:value={cfg.max_position_btc} step="0.001" min="0.001" />
      </label>
      <label class="settings-field">日次最大損失率
        <input type="number" bind:value={cfg.max_daily_drawdown} step="0.01" min="0" max="1" />
      </label>
      <label class="settings-field">ストップロス率
        <input type="number" bind:value={cfg.stop_loss_pct} step="0.01" min="0" max="1" />
      </label>
      <label class="settings-field">最小注文サイズ(BTC)
        <input type="number" bind:value={cfg.min_order_size} step="0.001" min="0.001" />
      </label>
    </div>

    <h3>シグナル</h3>
    <div class="settings-grid">
      <label class="settings-field">シグナル閾値
        <input type="number" bind:value={cfg.signal_threshold} step="0.05" min="0" max="1" />
      </label>
      <label class="settings-field" title="この幅未満の配分変更は注文しない">配分閾値(dead-band)
        <input type="number" bind:value={cfg.allocation_threshold} step="0.01" min="0" max="1" />
      </label>
      <label class="settings-field">TA重み
        <input type="number" bind:value={cfg.ta_weight} step="0.05" min="0" max="1" />
      </label>
      <label class="settings-field">センチメント重み
        <input type="number" bind:value={cfg.sentiment_weight} step="0.05" min="0" max="1" />
      </label>
    </div>

    <h3>インジケータ</h3>
    <div class="settings-grid">
      <label class="settings-field">SMA期間
        <input type="number" bind:value={cfg.sma_period} min="2" step="1" />
      </label>
      <label class="settings-field">EMA期間
        <input type="number" bind:value={cfg.ema_period} min="2" step="1" />
      </label>
      <label class="settings-field">RSI期間
        <input type="number" bind:value={cfg.rsi_period} min="2" step="1" />
      </label>
      <label class="settings-field">MACD Fast
        <input type="number" bind:value={cfg.macd_fast} min="2" step="1" />
      </label>
      <label class="settings-field">MACD Slow
        <input type="number" bind:value={cfg.macd_slow} min="3" step="1" />
      </label>
      <label class="settings-field">MACDシグナル
        <input type="number" bind:value={cfg.macd_signal} min="2" step="1" />
      </label>
      <label class="settings-field">ボリンジャー期間
        <input type="number" bind:value={cfg.bollinger_period} min="2" step="1" />
      </label>
      <label class="settings-field">ボリンジャー標準偏差
        <input type="number" bind:value={cfg.bollinger_std} step="0.1" min="0.1" />
      </label>
    </div>

    <div class="save-row">
      <button class="save-btn" on:click={handleSave} disabled={saving}>保存</button>
      {#if msg}
        <span class="save-msg" class:ok={msgOk} class:err={!msgOk}>{msg}</span>
      {/if}
    </div>
  {:else}
    <p class="loading">設定を読み込み中...</p>
  {/if}
</section>

<style>
  .settings-section {
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
  h3 {
    font-size: 12px;
    color: #888;
    margin: 12px 0 8px;
  }
  .settings-grid {
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    margin-bottom: 12px;
  }
  .settings-field {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 12px;
    color: #888;
  }
  .settings-field input {
    background: #0f3460;
    border: 1px solid #26a69a;
    color: #e0e0e0;
    padding: 4px 8px;
    border-radius: 4px;
    width: 160px;
  }
  .save-row {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-top: 4px;
  }
  .save-btn {
    background: #26a69a;
    color: #fff;
    border: none;
    padding: 8px 20px;
    border-radius: 4px;
    cursor: pointer;
    font-size: 13px;
  }
  .save-btn:hover { background: #1e8a7e; }
  .save-btn:disabled { opacity: 0.5; cursor: not-allowed; }
  .save-msg { font-size: 12px; }
  .save-msg.ok { color: #26a69a; }
  .save-msg.err { color: #ef5350; }
  .loading { color: #888; font-size: 12px; }

  @media (max-width: 767px) {
    .settings-field input {
      width: 130px;
    }
  }
</style>

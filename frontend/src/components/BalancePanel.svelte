<script lang="ts">
  import { balances, currentPrice, feeRate } from '../lib/stores';

  $: jpy = $balances.find(b => b.currency_code === 'JPY');
  $: btc = $balances.find(b => b.currency_code === 'BTC');

  $: jpyAmount = jpy ? Number(jpy.amount) : null;
  $: jpyAvail = jpy ? Number(jpy.available) : null;
  $: jpyLocked = jpyAmount !== null && jpyAvail !== null ? jpyAmount - jpyAvail : null;

  $: btcAmount = btc ? Number(btc.amount) : null;
  $: btcAvail = btc ? Number(btc.available) : null;
  $: btcLocked = btcAmount !== null && btcAvail !== null ? btcAmount - btcAvail : null;

  $: btcJpy = btcAvail !== null && $currentPrice !== null ? btcAvail * $currentPrice : null;
  $: totalJpy = jpyAvail !== null && btcJpy !== null ? jpyAvail + btcJpy : null;

  $: jpyPct = totalJpy !== null && jpyAvail !== null && totalJpy > 0
    ? (jpyAvail / totalJpy * 100).toFixed(1)
    : null;
  $: btcPct = totalJpy !== null && btcJpy !== null && totalJpy > 0
    ? (btcJpy / totalJpy * 100).toFixed(1)
    : null;

  function fmt(n: number | null, decimals = 0): string {
    if (n === null) return '---';
    return n.toLocaleString('ja-JP', { maximumFractionDigits: decimals });
  }
</script>

<div class="panel">
  <h2>残高</h2>
  <div class="balance-grid">
    <div class="balance-item">
      <div class="balance-currency">JPY</div>
      <div class="balance-row"><span class="lbl">保有</span><span>{fmt(jpyAmount)}</span></div>
      <div class="balance-row"><span class="lbl">利用可</span><span>{fmt(jpyAvail)}</span></div>
      <div class="balance-row locked"><span class="lbl">ロック中</span><span>{jpyLocked !== null && jpyLocked > 0 ? fmt(jpyLocked) : '0'}</span></div>
      <div class="balance-pct">{jpyPct !== null ? jpyPct + '%' : '--.--%'}</div>
    </div>
    <div class="balance-item">
      <div class="balance-currency">BTC</div>
      <div class="balance-row"><span class="lbl">保有</span><span>{btcAmount !== null ? btcAmount.toFixed(6) : '---'}</span></div>
      <div class="balance-row"><span class="lbl">利用可</span><span>{btcAvail !== null ? btcAvail.toFixed(6) : '---'}</span></div>
      <div class="balance-row locked"><span class="lbl">ロック中</span><span>{btcLocked !== null && btcLocked > 0 ? btcLocked.toFixed(6) : '0.000000'}</span></div>
      <div class="balance-btc-jpy">≈ {btcJpy !== null ? Math.round(btcJpy).toLocaleString('ja-JP') : '---'}円</div>
      <div class="balance-pct">{btcPct !== null ? btcPct + '%' : '--.--%'}</div>
    </div>
  </div>
  <div class="balance-total">
    <span class="balance-total-label">総残高（円）</span>
    <span class="balance-total-amount">{totalJpy !== null ? Math.round(totalJpy).toLocaleString('ja-JP') : '---'}</span>
  </div>
  <div class="balance-fee">
    <span class="fee-label">取引手数料（Taker）</span>
    <span class="fee-pct">{($feeRate * 100).toFixed(4)}%</span>
  </div>
</div>

<style>
  .panel {
    background: #16213e;
    border-radius: 8px;
    padding: 12px;
  }
  h2 {
    font-size: 13px;
    color: #888;
    margin: 0 0 10px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }
  .balance-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }
  .balance-item {
    background: #0f3460;
    border-radius: 6px;
    padding: 10px;
  }
  .balance-currency {
    font-size: 11px;
    color: #888;
  }
  .balance-row {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    margin-top: 3px;
    font-size: 13px;
    font-weight: bold;
    color: #e0e0e0;
  }
  .balance-row .lbl {
    font-size: 10px;
    color: #666;
    font-weight: normal;
  }
  .balance-row.locked span:last-child {
    font-size: 12px;
    color: #ef9a9a;
    font-weight: normal;
  }
  .balance-btc-jpy {
    font-size: 11px;
    color: #aaa;
    margin-top: 2px;
  }
  .balance-pct {
    font-size: 11px;
    color: #888;
    margin-top: 1px;
  }
  .balance-total {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-top: 10px;
    padding-top: 10px;
    border-top: 1px solid #333;
  }
  .balance-total-label {
    color: #888;
    font-size: 0.85em;
  }
  .balance-total-amount {
    font-size: 1.2em;
    font-weight: bold;
    color: #e0e0e0;
  }
  .balance-fee {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-top: 8px;
    padding-top: 8px;
    border-top: 1px solid #333;
  }
  .fee-label {
    color: #888;
    font-size: 0.82em;
  }
  .fee-pct {
    font-size: 0.95em;
    color: #ffd54f;
    font-weight: bold;
  }
</style>

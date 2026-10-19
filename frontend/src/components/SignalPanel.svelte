<script lang="ts">
  import { signal } from '../lib/stores';

  function signalClass(pct: number): string {
    if (pct > 0.55) return 'signal-buy';
    if (pct < 0.45) return 'signal-sell';
    return 'signal-hold';
  }

  function formatIndicatorValue(name: string, value: number): string {
    if (name === 'RSI') return value.toFixed(1);
    if (name === 'MACD') return value.toFixed(2);
    if (name === 'Bollinger') return `%B ${value.toFixed(1)}`;
    return value > 1000 ? `¥${Math.round(value).toLocaleString('ja-JP')}` : value.toFixed(4);
  }

  $: detail = $signal;
  $: targetPct = detail ? (detail.target_pct * 100).toFixed(1) : '--.-';
  $: gaugeWidth = detail ? `${(detail.target_pct * 100).toFixed(1)}%` : '50%';
  $: targetClass = detail ? signalClass(detail.target_pct) : 'signal-hold';
  $: rawSignal = detail ? (detail.aggregate.raw_signal ?? detail.target_pct) : null;
  $: showZoneDiff = detail && rawSignal !== null && Math.abs(rawSignal - detail.target_pct) >= 0.01;

  function zoneBadge(target: number, raw: number): { text: string; cls: string } {
    if (target <= 0.001 && raw > 0.001) return { text: 'Zone A  JPY保持', cls: 'zone-badge-a' };
    if (target >= 0.999 && raw < 0.999) return { text: 'Zone C  BTC保持', cls: 'zone-badge-c' };
    return { text: 'Zone B  比例配分', cls: 'zone-badge-b' };
  }

  function calcTimeStr(isoStr: string): string {
    const d = new Date(isoStr);
    return d.toLocaleTimeString('ja-JP', { hour12: false }) +
      '.' + String(d.getMilliseconds()).padStart(3, '0');
  }
</script>

<div class="panel">
  <h2>シグナル</h2>

  <div class="signal-composite">
    <span class="signal-composite-label">目標BTC配分</span>
    <span class="alloc-target-pct {targetClass}">{targetPct}%</span>
  </div>

  {#if showZoneDiff && detail && rawSignal !== null}
    {@const badge = zoneBadge(detail.target_pct, rawSignal)}
    <div class="signal-zone-diff">
      <span class="zone-raw-label">生シグナル:</span>
      <span class="zone-raw-val">{(rawSignal * 100).toFixed(1)}%</span>
      <span class="zone-arrow">→</span>
      <span class="zone-badge {badge.cls}">{badge.text}</span>
    </div>
  {/if}

  <div class="alloc-gauge-wrap">
    <div class="alloc-gauge-track">
      <div class="alloc-gauge-fill" style="width: {gaugeWidth}"></div>
    </div>
    <div class="alloc-labels"><span>0%</span><span>50%</span><span>100%</span></div>
  </div>

  <div class="signal-confidence">
    {#if detail}
      配分率 {(detail.aggregate.normalized * 100).toFixed(0)}%
    {:else}
      シグナル待機中
    {/if}
  </div>

  <div class="calc-state">
    <span
      class="calc-dot"
      class:active={detail?.calculation_state === 'active'}
      class:waiting={!detail || detail.calculation_state !== 'active'}
    ></span>
    <span class="calc-state-text">
      {#if !detail}シグナル待機中
      {:else if detail.calculation_state === 'active'}計算中
      {:else}データ待ち
      {/if}
    </span>
    {#if detail?.calculated_at}
      <span class="calc-time">{calcTimeStr(detail.calculated_at)}</span>
    {/if}
  </div>

  <div class="signal-rows">
    {#if detail}
      {#each detail.indicators as ind}
        <div class="signal-row">
          <span class="signal-name">{ind.name}</span>
          <span class="signal-detail">
            {ind.value != null ? formatIndicatorValue(ind.name, ind.value) : '---'}
          </span>
        </div>
      {/each}
    {/if}
  </div>
</div>

<style>
  .panel {
    background: #16213e;
    border-radius: 8px;
    padding: 12px;
    overflow-y: auto;
  }
  h2 {
    font-size: 13px;
    color: #888;
    margin: 0 0 10px;
    text-transform: uppercase;
    letter-spacing: 0.5px;
  }
  .signal-composite {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 8px;
    padding-bottom: 8px;
    border-bottom: 1px solid #0f3460;
  }
  .signal-composite-label {
    font-size: 11px;
    color: #888;
  }
  .alloc-target-pct {
    font-size: 26px;
    font-weight: bold;
  }
  .signal-buy { color: #26a69a; }
  .signal-sell { color: #ef5350; }
  .signal-hold { color: #888; }

  .signal-zone-diff {
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
    font-size: 11px;
    color: #aaa;
    margin-bottom: 8px;
  }
  .zone-raw-label { color: #777; }
  .zone-raw-val { color: #ccc; font-weight: bold; }
  .zone-arrow { color: #555; }
  .zone-badge {
    font-size: 10px;
    font-weight: bold;
    padding: 1px 6px;
    border-radius: 3px;
  }
  .zone-badge-a { background: #4a1515; color: #ef5350; border: 1px solid #ef5350; }
  .zone-badge-b { background: #3a3000; color: #ffb300; border: 1px solid #ffb300; }
  .zone-badge-c { background: #0d2e2b; color: #26a69a; border: 1px solid #26a69a; }

  .alloc-gauge-wrap { margin-bottom: 10px; }
  .alloc-gauge-track {
    background: #0f3460;
    border-radius: 4px;
    height: 10px;
    position: relative;
  }
  .alloc-gauge-fill {
    height: 100%;
    border-radius: 4px;
    background: linear-gradient(to right, #ef5350, #ffb300, #26a69a);
    transition: width 0.4s;
  }
  .alloc-labels {
    display: flex;
    justify-content: space-between;
    font-size: 10px;
    color: #555;
    margin-top: 3px;
  }

  .signal-confidence {
    font-size: 12px;
    color: #888;
    margin-bottom: 8px;
  }

  .calc-state {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: 4px 0 6px;
    font-size: 11px;
    color: #888;
  }
  .calc-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #555;
    flex-shrink: 0;
  }
  .calc-dot.active {
    background: #26a69a;
    animation: calcPulse 1s infinite;
  }
  .calc-dot.waiting { background: #555; }
  @keyframes calcPulse {
    0%, 100% { opacity: 1; }
    50% { opacity: 0.3; }
  }
  .calc-state-text { flex: 1; }
  .calc-time {
    margin-left: auto;
    font-family: monospace;
    font-size: 10px;
  }

  .signal-rows { margin-top: 4px; }
  .signal-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 5px 0;
    border-bottom: 1px solid #1a1a2e;
    font-size: 12px;
  }
  .signal-row:last-child { border-bottom: none; }
  .signal-name { color: #888; min-width: 40px; }
  .signal-detail { color: #ccc; flex: 1; margin: 0 8px; font-size: 11px; }
</style>

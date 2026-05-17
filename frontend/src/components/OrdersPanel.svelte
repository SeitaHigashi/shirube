<script lang="ts">
  import { orders } from '../lib/stores';

  const JST_OFFSET = 9 * 3600 * 1000;

  function formatTimeBoth(isoStr: string): string {
    const d = new Date(isoStr);
    const utc = d.toISOString().slice(11, 16);
    const jst = new Date(d.getTime() + JST_OFFSET).toISOString().slice(11, 16);
    return `${utc}Z (${jst} JST)`;
  }

  $: recentOrders = $orders.slice(0, 20);
</script>

<div class="panel">
  <h2>最近の注文</h2>
  <table>
    <thead>
      <tr>
        <th>時刻</th>
        <th>方向</th>
        <th>サイズ</th>
        <th>価格</th>
        <th>状態</th>
      </tr>
    </thead>
    <tbody>
      {#if recentOrders.length === 0}
        <tr><td colspan="5" class="empty">データなし</td></tr>
      {:else}
        {#each recentOrders as o}
          <tr>
            <td class="time">{formatTimeBoth(o.created_at)}</td>
            <td class={o.side.toLowerCase()}>{o.side === 'BUY' ? '買' : '売'}</td>
            <td>{Number(o.size).toFixed(4)}</td>
            <td>{o.price ? Number(o.price).toLocaleString('ja-JP') : 'MARKET'}</td>
            <td class="status">{o.status}</td>
          </tr>
        {/each}
      {/if}
    </tbody>
  </table>
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
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 12px;
  }
  th {
    color: #888;
    font-weight: normal;
    text-align: left;
    padding: 4px 6px;
    border-bottom: 1px solid #0f3460;
  }
  td {
    padding: 4px 6px;
    border-bottom: 1px solid #1a1a2e;
    color: #e0e0e0;
  }
  .buy { color: #26a69a; }
  .sell { color: #ef5350; }
  .time {
    color: #666;
    font-size: 11px;
    white-space: nowrap;
  }
  .status {
    color: #888;
    font-size: 11px;
  }
  .empty {
    color: #888;
    text-align: center;
  }
</style>

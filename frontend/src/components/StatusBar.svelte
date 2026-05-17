<script lang="ts">
  import { wsStatus, lastUpdate } from '../lib/stores';
</script>

<div class="status-bar">
  <span>WebSocket: <span class="ws-status" class:connected={$wsStatus === 'connected'}>
    {$wsStatus === 'connected' ? '接続中' : '切断'}
  </span></span>
  <span>{$lastUpdate}</span>
</div>

<style>
  .status-bar {
    position: fixed;
    bottom: 0;
    left: 0;
    right: 0;
    background: #0f3460;
    padding: 4px 16px;
    font-size: 11px;
    color: #888;
    display: flex;
    gap: 16px;
    z-index: 100;
  }
  .ws-status {
    color: #ef5350;
  }
  .ws-status.connected {
    color: #26a69a;
  }
</style>

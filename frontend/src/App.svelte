<script lang="ts">
  import { onMount } from 'svelte';
  import Header from './components/Header.svelte';
  import ChartPanel from './components/ChartPanel.svelte';
  import SignalPanel from './components/SignalPanel.svelte';
  import BalancePanel from './components/BalancePanel.svelte';
  import OrdersPanel from './components/OrdersPanel.svelte';
  import NewsPanel from './components/NewsPanel.svelte';
  import SettingsPanel from './components/SettingsPanel.svelte';
  import StatusBar from './components/StatusBar.svelte';
  import { initWebSockets } from './lib/ws';
  import { fetchBalance, fetchOrders, fetchSignal, fetchNews, fetchFee } from './lib/api';
  import { balances, orders, signal, newsItems, feeRate } from './lib/stores';

  onMount(async () => {
    initWebSockets(60);

    try {
      const [bal, ord, sig, news, fee] = await Promise.all([
        fetchBalance(),
        fetchOrders(),
        fetchSignal(),
        fetchNews(),
        fetchFee(),
      ]);
      balances.set(bal);
      orders.set(ord);
      if (sig) signal.set(sig);
      newsItems.set(news);
      feeRate.set(fee.fee_pct);
    } catch (e) {
      console.warn('Initial data load failed', e);
    }

    // Polling for data that doesn't come via WebSocket
    const balanceTimer = setInterval(async () => {
      try { balances.set(await fetchBalance()); } catch (_) {}
    }, 15000);

    const ordersTimer = setInterval(async () => {
      try { orders.set(await fetchOrders()); } catch (_) {}
    }, 10000);

    const newsTimer = setInterval(async () => {
      try { newsItems.set(await fetchNews()); } catch (_) {}
    }, 30000);

    return () => {
      clearInterval(balanceTimer);
      clearInterval(ordersTimer);
      clearInterval(newsTimer);
    };
  });
</script>

<div class="app">
  <Header />

  <main class="main-layout">
    <div class="chart-wrapper">
      <ChartPanel />
    </div>
    <aside class="sidebar">
      <SignalPanel />
      <BalancePanel />
      <OrdersPanel />
    </aside>
  </main>

  <NewsPanel />
  <SettingsPanel />
  <StatusBar />
</div>

<style>
  :global(*) {
    box-sizing: border-box;
    margin: 0;
    padding: 0;
  }
  :global(body) {
    background: #1a1a2e;
    color: #e0e0e0;
    font-family: 'Segoe UI', sans-serif;
    font-size: 14px;
    padding-bottom: 32px;
  }

  .app {
    display: flex;
    flex-direction: column;
    min-height: 100vh;
  }

  /* Desktop: side-by-side, chart fills remaining height */
  .main-layout {
    display: flex;
    gap: 12px;
    padding: 12px;
    /* Subtract header (~56px) + status bar (32px) */
    height: calc(100vh - 88px);
    box-sizing: border-box;
  }

  .chart-wrapper {
    flex: 1;
    min-width: 0;
    min-height: 0;
    overflow: hidden;
  }

  .sidebar {
    width: 320px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    gap: 12px;
    overflow-y: auto;
    scrollbar-width: thin;
    scrollbar-color: #0f3460 transparent;
  }
  .sidebar::-webkit-scrollbar { width: 4px; }
  .sidebar::-webkit-scrollbar-thumb { background: #0f3460; border-radius: 2px; }

  /* Mobile: vertical stack */
  @media (max-width: 767px) {
    .main-layout {
      flex-direction: column;
      height: auto;
      padding: 8px;
      gap: 8px;
    }

    .chart-wrapper {
      height: 420px;
      flex: none;
    }

    .sidebar {
      width: 100%;
      overflow-y: visible;
      gap: 8px;
    }
  }
</style>

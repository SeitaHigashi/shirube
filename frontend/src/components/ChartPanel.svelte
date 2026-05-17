<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { createChart, CrosshairMode } from 'lightweight-charts';
  import { wsCandleUpdate, currentPrice, feeRate } from '../lib/stores';
  import { fetchCandles, fetchIndicators } from '../lib/api';
  import { reconnectCandleWS } from '../lib/ws';
  import type { CandleBar } from '../lib/types';

  // Resolution config
  const RESOLUTIONS = [
    { key: 60,    label: '1分',   title: '1分足' },
    { key: 300,   label: '5分',   title: '5分足' },
    { key: 900,   label: '15分',  title: '15分足' },
    { key: 3600,  label: '1時間', title: '1時間足' },
    { key: 86400, label: '日足',  title: '日足' },
  ];
  const RES_COUNT: Record<number, number> = {
    60: 200, 300: 200, 900: 200, 3600: 200, 86400: 60,
  };

  let resolution = 60;
  let chartTitle = '1分足';

  // DOM refs
  let containerEl: HTMLDivElement;
  let chartEl: HTMLDivElement;
  let rsiEl: HTMLDivElement;
  let macdEl: HTMLDivElement;
  let rsiLabelEl: HTMLElement;
  let macdLabelEl: HTMLElement;

  // Chart instances (imperative lightweight-charts objects)
  let chart: ReturnType<typeof createChart>;
  let rsiChart: ReturnType<typeof createChart>;
  let macdChart: ReturnType<typeof createChart>;
  let candleSeries: any;
  let newsFixedSeries: any;
  let smaSeries: any;
  let emaSeries: any;
  let bbUpperSeries: any;
  let bbMiddleSeries: any;
  let bbLowerSeries: any;
  let rsiSeries: any;
  let rsiOBSeries: any;
  let rsiOSSeries: any;
  let macdLineSeries: any;
  let macdSignalSeries: any;
  let macdHistSeries: any;

  // State
  let candleData: CandleBar[] = [];
  let isLoadingHistory = false;
  let hasMoreHistory = true;
  let bepUpperLine: any = null;
  let bepLowerLine: any = null;
  let indicatorTimer: ReturnType<typeof setTimeout> | null = null;
  let _feeRate = 0.0015;
  let _currentPrice: number | null = null;

  // Store unsubscribers
  let unsubCandle: () => void;
  let unsubPrice: () => void;
  let unsubFee: () => void;

  function toChartTime(isoStr: string): number {
    return Math.floor(new Date(isoStr).getTime() / 1000);
  }

  function makeTzFormatter() {
    return (ts: number) => {
      const d = new Date(ts * 1000);
      return d.toISOString().slice(11, 16) + 'Z';
    };
  }

  function baseChartOpts() {
    return {
      layout: { background: { color: '#16213e' }, textColor: '#e0e0e0' },
      grid: { vertLines: { color: '#0f3460' }, horzLines: { color: '#0f3460' } },
      crosshair: { mode: CrosshairMode.Normal },
      rightPriceScale: { borderColor: '#0f3460', minimumWidth: 90 },
      timeScale: { borderColor: '#0f3460', timeVisible: true, secondsVisible: false },
      handleScale: { mouseWheel: false },
      localization: { timeFormatter: makeTzFormatter() },
    };
  }

  // Cursor-anchored wheel zoom — same logic as the original implementation
  function applyZoom(e: WheelEvent) {
    const range = chart.timeScale().getVisibleLogicalRange();
    if (!range) return;
    const priceScaleWidth = 90;
    const chartWidth = chartEl.clientWidth - priceScaleWidth;
    const cursorX = Math.max(0, Math.min(e.offsetX, chartWidth));
    const fraction = cursorX / chartWidth;
    const visibleBars = range.to - range.from;
    const cursorLogical = range.from + fraction * visibleBars;
    const zoomFactor = e.deltaY > 0 ? 1.12 : 0.88;
    const newVisibleBars = visibleBars * zoomFactor;
    const newFrom = cursorLogical - fraction * newVisibleBars;
    chart.timeScale().setVisibleLogicalRange({ from: newFrom, to: range.to });
  }

  function resizeCharts() {
    if (!chart || !containerEl || !chartEl) return;
    const headerH = containerEl.querySelector('.chart-header')?.clientHeight ?? 0;
    const legendH = containerEl.querySelector('.indicator-legend')?.clientHeight ?? 0;
    const rsiLabelH = rsiLabelEl?.offsetHeight ?? 0;
    const macdLabelH = macdLabelEl?.offsetHeight ?? 0;
    const rsiH = rsiEl.offsetHeight + rsiLabelH + 4;
    const macdH = macdEl.offsetHeight + macdLabelH + 4;
    const totalPad = 24;
    const available = containerEl.clientHeight - headerH - legendH - rsiH - macdH - totalPad - 8;
    chart.applyOptions({ width: chartEl.clientWidth, height: Math.max(100, available) });
    rsiChart.applyOptions({ width: rsiEl.clientWidth });
    macdChart.applyOptions({ width: macdEl.clientWidth });
  }

  async function loadCandles() {
    isLoadingHistory = false;
    hasMoreHistory = true;
    candleData = [];
    try {
      const count = RES_COUNT[resolution] ?? 200;
      const candles = await fetchCandles(resolution, count);
      const data = candles
        .map(c => ({
          time: toChartTime(c.open_time),
          open: parseFloat(c.open),
          high: parseFloat(c.high),
          low: parseFloat(c.low),
          close: parseFloat(c.close),
        }))
        .sort((a, b) => a.time - b.time);
      if (data.length > 0) {
        candleData = data;
        candleSeries.setData(data);
        chart.timeScale().fitContent();
        rsiChart.timeScale().fitContent();
        macdChart.timeScale().fitContent();
        await loadAndRenderIndicators();
      }
    } catch (e) { console.warn('candles load failed', e); }
  }

  async function loadMoreHistory() {
    if (isLoadingHistory || !hasMoreHistory || candleData.length === 0) return;
    isLoadingHistory = true;
    const to = new Date(candleData[0].time * 1000).toISOString();
    const count = RES_COUNT[resolution] ?? 200;
    try {
      const candles = await fetchCandles(resolution, count, to);
      if (candles.length === 0) { hasMoreHistory = false; return; }
      const newData = candles
        .map(c => ({
          time: toChartTime(c.open_time),
          open: parseFloat(c.open),
          high: parseFloat(c.high),
          low: parseFloat(c.low),
          close: parseFloat(c.close),
        }))
        .filter(c => c.time < candleData[0].time)
        .sort((a, b) => a.time - b.time);
      if (newData.length === 0) { hasMoreHistory = false; return; }
      const savedRange = chart.timeScale().getVisibleLogicalRange();
      candleData = [...newData, ...candleData];
      if (savedRange) {
        const offset = newData.length;
        candleSeries.setData(candleData);
        chart.timeScale().setVisibleLogicalRange({
          from: savedRange.from + offset,
          to: savedRange.to + offset,
        });
      }
      await loadAndRenderIndicators();
    } catch (e) {
      console.warn('History load error', e);
    } finally {
      isLoadingHistory = false;
    }
  }

  async function loadAndRenderIndicators() {
    try {
      const count = RES_COUNT[resolution] ?? 200;
      const points = await fetchIndicators(resolution, count);
      if (!points || points.length === 0) return;

      const savedRange = chart.timeScale().getVisibleLogicalRange();
      const toT = (p: any) => toChartTime(p.time);

      smaSeries.applyOptions({ visible: true });
      smaSeries.setData(points.filter(p => p.sma != null).map(p => ({ time: toT(p), value: p.sma })));

      emaSeries.applyOptions({ visible: true });
      emaSeries.setData(points.filter(p => p.ema != null).map(p => ({ time: toT(p), value: p.ema })));

      bbUpperSeries.applyOptions({ visible: true });
      bbMiddleSeries.applyOptions({ visible: true });
      bbLowerSeries.applyOptions({ visible: true });
      bbUpperSeries.setData(points.filter(p => p.bb_upper != null).map(p => ({ time: toT(p), value: p.bb_upper })));
      bbMiddleSeries.setData(points.filter(p => p.bb_middle != null).map(p => ({ time: toT(p), value: p.bb_middle })));
      bbLowerSeries.setData(points.filter(p => p.bb_lower != null).map(p => ({ time: toT(p), value: p.bb_lower })));

      if (savedRange) chart.timeScale().setVisibleLogicalRange(savedRange);
      resizeCharts();
    } catch (e) { console.warn('indicator load failed', e); }
  }

  function debouncedRenderIndicators() {
    if (indicatorTimer) clearTimeout(indicatorTimer);
    indicatorTimer = setTimeout(loadAndRenderIndicators, 3000);
  }

  function switchResolution(res: number) {
    resolution = res;
    chartTitle = RESOLUTIONS.find(r => r.key === res)?.title ?? `${res}秒足`;
    loadCandles();
    reconnectCandleWS(res);
  }

  function updateBepLines(price: number, fee: number) {
    if (!price || price <= 0 || !candleSeries) return;
    const upper = price * (1 + fee * 2);
    const lower = price * (1 - fee * 2);
    if (!bepUpperLine) {
      bepUpperLine = candleSeries.createPriceLine({
        price: upper, color: '#26a69a', lineWidth: 1, lineStyle: 2,
        axisLabelVisible: true, title: 'BEP+',
      });
      bepLowerLine = candleSeries.createPriceLine({
        price: lower, color: '#ef5350', lineWidth: 1, lineStyle: 2,
        axisLabelVisible: true, title: 'BEP-',
      });
    } else {
      bepUpperLine.applyOptions({ price: upper });
      bepLowerLine.applyOptions({ price: lower });
    }
  }

  onMount(() => {
    // Main chart
    chart = createChart(chartEl, {
      ...baseChartOpts(),
      width: chartEl.clientWidth,
      height: chartEl.clientHeight,
    });

    candleSeries = chart.addCandlestickSeries({
      upColor: '#26a69a', downColor: '#ef5350',
      borderUpColor: '#26a69a', borderDownColor: '#ef5350',
      wickUpColor: '#26a69a', wickDownColor: '#ef5350',
    });

    // Hidden overlay series for news marker anchoring
    newsFixedSeries = chart.addLineSeries({
      priceScaleId: 'news-overlay',
      color: 'transparent',
      lineWidth: 0,
      priceLineVisible: false,
      lastValueVisible: false,
      crosshairMarkerVisible: false,
      autoscaleInfoProvider: () => ({
        priceRange: { minValue: 0, maxValue: 1 },
        margins: { above: 0, below: 0 },
      }),
    });
    chart.priceScale('news-overlay').applyOptions({
      scaleMargins: { top: 0, bottom: 0 },
      visible: false,
    });

    smaSeries = chart.addLineSeries({ color: '#f9c74f', lineWidth: 1, visible: false, priceLineVisible: false, lastValueVisible: false });
    emaSeries = chart.addLineSeries({ color: '#90e0ef', lineWidth: 1, visible: false, priceLineVisible: false, lastValueVisible: false });
    bbUpperSeries = chart.addLineSeries({ color: '#c77dff', lineWidth: 1, lineStyle: 2, visible: false, priceLineVisible: false, lastValueVisible: false });
    bbMiddleSeries = chart.addLineSeries({ color: '#c77dff', lineWidth: 1, visible: false, priceLineVisible: false, lastValueVisible: false });
    bbLowerSeries = chart.addLineSeries({ color: '#c77dff', lineWidth: 1, lineStyle: 2, visible: false, priceLineVisible: false, lastValueVisible: false });

    // RSI sub-chart
    rsiChart = createChart(rsiEl, {
      ...baseChartOpts(),
      width: rsiEl.clientWidth,
      height: 100,
      rightPriceScale: { borderColor: '#0f3460', scaleMargins: { top: 0.1, bottom: 0.1 }, minimumWidth: 90 },
    });
    rsiSeries = rsiChart.addLineSeries({ color: '#ff9e6d', lineWidth: 1, priceLineVisible: false, lastValueVisible: false });
    rsiOBSeries = rsiChart.addLineSeries({ color: '#ef5350', lineWidth: 1, lineStyle: 2, priceLineVisible: false, lastValueVisible: false });
    rsiOSSeries = rsiChart.addLineSeries({ color: '#26a69a', lineWidth: 1, lineStyle: 2, priceLineVisible: false, lastValueVisible: false });

    // MACD sub-chart
    macdChart = createChart(macdEl, {
      ...baseChartOpts(),
      width: macdEl.clientWidth,
      height: 110,
      rightPriceScale: { borderColor: '#0f3460', scaleMargins: { top: 0.1, bottom: 0.1 }, minimumWidth: 90 },
    });
    macdLineSeries = macdChart.addLineSeries({ color: '#a8dadc', lineWidth: 1, priceLineVisible: false, lastValueVisible: false });
    macdSignalSeries = macdChart.addLineSeries({ color: '#e94560', lineWidth: 1, priceLineVisible: false, lastValueVisible: false });
    macdHistSeries = macdChart.addHistogramSeries({ color: '#26a69a', priceLineVisible: false, lastValueVisible: false });

    // Cursor-anchored zoom on all chart containers
    const wheelHandler = (e: Event) => { e.preventDefault(); applyZoom(e as WheelEvent); };
    chartEl.addEventListener('wheel', wheelHandler, { passive: false });
    rsiEl.addEventListener('wheel', wheelHandler, { passive: false });
    macdEl.addEventListener('wheel', wheelHandler, { passive: false });

    // Load more history when scrolled to the left edge
    chart.timeScale().subscribeVisibleLogicalRangeChange(range => {
      if (range !== null && range.from < 10) loadMoreHistory();
    });

    // Resize observer for the outer container
    const resizeObs = new ResizeObserver(resizeCharts);
    resizeObs.observe(containerEl);

    // Load initial candle data
    loadCandles();

    // Subscribe to candle WS updates
    unsubCandle = wsCandleUpdate.subscribe(bar => {
      if (!bar || !candleSeries) return;
      candleSeries.update(bar);
      const idx = candleData.findIndex(d => d.time === bar.time);
      if (idx >= 0) { candleData[idx] = bar; } else { candleData.push(bar); }
      debouncedRenderIndicators();
    });

    unsubFee = feeRate.subscribe(fee => {
      _feeRate = fee;
      if (_currentPrice) updateBepLines(_currentPrice, fee);
    });

    unsubPrice = currentPrice.subscribe(price => {
      _currentPrice = price;
      if (price) updateBepLines(price, _feeRate);
    });

    setTimeout(resizeCharts, 100);

    return () => {
      chartEl.removeEventListener('wheel', wheelHandler);
      rsiEl.removeEventListener('wheel', wheelHandler);
      macdEl.removeEventListener('wheel', wheelHandler);
      resizeObs.disconnect();
      if (indicatorTimer) clearTimeout(indicatorTimer);
      chart.remove();
      rsiChart.remove();
      macdChart.remove();
    };
  });

  onDestroy(() => {
    unsubCandle?.();
    unsubPrice?.();
    unsubFee?.();
  });
</script>

<div class="chart-container" bind:this={containerEl}>
  <div class="chart-header">
    <div class="chart-title">BTC/JPY — {chartTitle}</div>
    <div class="resolution-buttons">
      {#each RESOLUTIONS as res}
        <button
          class="res-btn"
          class:active={resolution === res.key}
          on:click={() => switchResolution(res.key)}
        >{res.label}</button>
      {/each}
    </div>
  </div>

  <div class="indicator-legend">
    <span class="leg-item"><span class="leg-dot" style="background:#f9c74f"></span>SMA</span>
    <span class="leg-item"><span class="leg-dot" style="background:#90e0ef"></span>EMA</span>
    <span class="leg-item"><span class="leg-dot" style="background:#c77dff"></span>BB</span>
    <span class="leg-item"><span class="leg-dot" style="background:#ffd166"></span>ニュース</span>
  </div>

  <div class="chart-main" bind:this={chartEl}></div>

  <div class="sub-label rsi-label" bind:this={rsiLabelEl}>RSI</div>
  <div class="sub-chart rsi-chart" bind:this={rsiEl}></div>
  <div class="sub-label macd-label" bind:this={macdLabelEl}>MACD</div>
  <div class="sub-chart macd-chart" bind:this={macdEl}></div>
</div>

<style>
  .chart-container {
    background: #16213e;
    border-radius: 8px;
    padding: 12px;
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 0;
    overflow: hidden;
    box-sizing: border-box;
  }
  .chart-header {
    display: flex;
    align-items: center;
    gap: 12px;
    margin-bottom: 4px;
    flex-shrink: 0;
  }
  .chart-title {
    font-size: 13px;
    color: #888;
  }
  .resolution-buttons {
    display: flex;
    gap: 4px;
    margin-left: auto;
    flex-wrap: wrap;
  }
  .res-btn {
    background: #1e2d4a;
    color: #aaa;
    border: 1px solid #2a3f60;
    border-radius: 4px;
    padding: 2px 10px;
    cursor: pointer;
    font-size: 11px;
  }
  .res-btn.active {
    background: #2a5298;
    color: #fff;
    border-color: #4a80d4;
  }
  .res-btn:hover:not(.active) { background: #253a5e; }

  .indicator-legend {
    display: flex;
    gap: 14px;
    padding: 4px 0 6px;
    flex-wrap: wrap;
    flex-shrink: 0;
  }
  .leg-item {
    display: flex;
    align-items: center;
    gap: 5px;
    font-size: 12px;
    color: #aaa;
  }
  .leg-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    display: inline-block;
    flex-shrink: 0;
  }

  .chart-main {
    width: 100%;
    flex: 1;
    min-height: 100px;
  }

  .sub-label {
    font-size: 10px;
    color: #ff9e6d;
    margin-top: 4px;
    display: block;
    flex-shrink: 0;
  }
  .macd-label { color: #a8dadc; }

  .sub-chart {
    width: 100%;
    height: 100px;
    display: block;
    margin-top: 4px;
    flex-shrink: 0;
  }
  .macd-chart { height: 110px; }
</style>

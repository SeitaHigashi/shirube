import { ticker, signal, wsStatus, lastUpdate, currentPrice, wsCandleUpdate } from './stores';
import type { CandleBar } from './types';

const WS_BASE = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}`;

const JST_OFFSET = 9 * 3600 * 1000;

export function toChartTime(isoStr: string): number {
  return Math.floor(new Date(isoStr).getTime() / 1000);
}

export function formatTimeBoth(isoStr: string): string {
  const d = new Date(isoStr);
  const utc = d.toISOString().slice(11, 16);
  const jst = new Date(d.getTime() + JST_OFFSET).toISOString().slice(11, 16);
  return `${utc}Z (${jst} JST)`;
}

// ── Candle WebSocket ─────────────────────────────────────────────────────────

let candleWs: WebSocket | null = null;
let _currentCandleResolution = 60;

export function reconnectCandleWS(resolution: number) {
  _currentCandleResolution = resolution;
  if (candleWs) {
    candleWs.onclose = null;
    candleWs.close();
    candleWs = null;
  }
  connectCandleWS(resolution);
}

function connectCandleWS(resolution: number) {
  const wsUrl = `${WS_BASE}/ws/candles?resolution=${resolution}`;
  candleWs = new WebSocket(wsUrl);

  candleWs.onopen = () => wsStatus.set('connected');

  candleWs.onmessage = (evt) => {
    try {
      const c = JSON.parse(evt.data);
      const bar: CandleBar = {
        time: toChartTime(c.open_time),
        open: parseFloat(c.open),
        high: parseFloat(c.high),
        low: parseFloat(c.low),
        close: parseFloat(c.close),
      };
      wsCandleUpdate.set(bar);
      currentPrice.set(bar.close);
      lastUpdate.set('最終更新: ' + formatTimeBoth(new Date().toISOString()));
    } catch (e) { console.warn('candle ws parse error', e); }
  };

  candleWs.onclose = () => {
    wsStatus.set('disconnected');
    setTimeout(() => connectCandleWS(_currentCandleResolution), 3000);
  };
  candleWs.onerror = () => candleWs?.close();
}

// ── Ticker WebSocket ─────────────────────────────────────────────────────────

let tickerWs: WebSocket | null = null;

function connectTickerWS() {
  tickerWs = new WebSocket(`${WS_BASE}/ws/tickers`);

  tickerWs.onmessage = (evt) => {
    try {
      const t = JSON.parse(evt.data);
      ticker.set(t);
      currentPrice.set(Number(t.ltp));
      lastUpdate.set('最終更新: ' + formatTimeBoth(new Date().toISOString()));
    } catch (e) { console.warn('ticker ws parse error', e); }
  };

  tickerWs.onclose = () => setTimeout(connectTickerWS, 3000);
  tickerWs.onerror = () => tickerWs?.close();
}

// ── Signal WebSocket ─────────────────────────────────────────────────────────

let signalWs: WebSocket | null = null;

function connectSignalWS() {
  signalWs = new WebSocket(`${WS_BASE}/ws/signal`);

  signalWs.onmessage = (evt) => {
    try {
      signal.set(JSON.parse(evt.data));
    } catch (e) { console.warn('signal ws parse error', e); }
  };

  signalWs.onclose = () => setTimeout(connectSignalWS, 3000);
  signalWs.onerror = () => signalWs?.close();
}

// ── Init ─────────────────────────────────────────────────────────────────────

export function initWebSockets(initialResolution: number) {
  _currentCandleResolution = initialResolution;
  connectCandleWS(initialResolution);
  connectTickerWS();
  connectSignalWS();
}

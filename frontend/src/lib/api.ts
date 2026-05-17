import type {
  Ticker, Candle, Balance, SignalDetail, Order,
  NewsItem, TradingConfig, IndicatorPoint, FeeInfo,
} from './types';

const BASE = `${location.protocol}//${location.host}`;

export async function fetchTicker(): Promise<Ticker> {
  const res = await fetch(`${BASE}/api/ticker`);
  return res.json();
}

export async function fetchCandles(
  resolution: number,
  count: number,
  to?: string,
): Promise<Candle[]> {
  let url = `${BASE}/api/candles?resolution=${resolution}&count=${count}`;
  if (to) url += `&to=${encodeURIComponent(to)}`;
  const res = await fetch(url);
  return res.json();
}

export async function fetchBalance(): Promise<Balance[]> {
  const res = await fetch(`${BASE}/api/balance`);
  return res.json();
}

export async function fetchOrders(): Promise<Order[]> {
  const res = await fetch(`${BASE}/api/orders`);
  return res.json();
}

export async function fetchSignal(): Promise<SignalDetail | null> {
  const res = await fetch(`${BASE}/api/signal/latest`);
  if (!res.ok) return null;
  return res.json();
}

export async function fetchNews(): Promise<NewsItem[]> {
  const res = await fetch(`${BASE}/api/news/latest`);
  return res.json();
}

export async function fetchConfig(): Promise<TradingConfig> {
  const res = await fetch(`${BASE}/api/config`);
  return res.json();
}

export async function saveConfig(
  cfg: TradingConfig,
): Promise<{ ok: boolean; error?: string }> {
  const res = await fetch(`${BASE}/api/config`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(cfg),
  });
  if (!res.ok) {
    const err = await res.json().catch(() => ({}));
    return { ok: false, error: (err as any).error || String(res.status) };
  }
  return { ok: true };
}

export async function fetchIndicators(
  resolution: number,
  count: number,
): Promise<IndicatorPoint[]> {
  const res = await fetch(`${BASE}/api/indicators?resolution=${resolution}&count=${count}`);
  return res.json();
}

export async function fetchFee(): Promise<FeeInfo> {
  const res = await fetch(`${BASE}/api/fee`);
  return res.json();
}

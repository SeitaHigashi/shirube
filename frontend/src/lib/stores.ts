import { writable } from 'svelte/store';
import type { Ticker, SignalDetail, NewsItem, Order, Balance, CandleBar } from './types';

export const ticker = writable<Ticker | null>(null);
export const signal = writable<SignalDetail | null>(null);
export const newsItems = writable<NewsItem[]>([]);
export const orders = writable<Order[]>([]);
export const balances = writable<Balance[]>([]);
export const feeRate = writable<number>(0.0015);
export const currentPrice = writable<number | null>(null);

// WebSocket state
export const wsStatus = writable<'connected' | 'disconnected'>('disconnected');
export const lastUpdate = writable<string>('');

// Candle WS updates: written by ws.ts, consumed by ChartPanel
export const wsCandleUpdate = writable<CandleBar | null>(null);

export interface Ticker {
  ltp: number;
  best_bid: number;
  best_ask: number;
  volume_by_product: number;
}

export interface Candle {
  open_time: string;
  open: string;
  high: string;
  low: string;
  close: string;
}

export interface Balance {
  currency_code: string;
  amount: number;
  available: number;
}

export interface IndicatorInfo {
  name: string;
  value: number | null;
}

export interface AggregateSignal {
  raw_signal: number | null;
  normalized: number;
}

export interface SignalDetail {
  target_pct: number;
  aggregate: AggregateSignal;
  calculation_state: 'active' | 'waiting';
  calculated_at: string | null;
  indicators: IndicatorInfo[];
}

export interface Order {
  created_at: string;
  side: 'BUY' | 'SELL';
  size: number;
  price: number | null;
  status: string;
}

export interface NewsItem {
  score: number;
  headline: string;
  published_at: string | null;
  analyzed_at: string | null;
}

export interface TradingConfig {
  max_position_btc: number;
  max_daily_drawdown: number;
  stop_loss_pct: number;
  min_order_size: number;
  signal_threshold: number;
  allocation_threshold: number;
  ta_weight: number;
  sentiment_weight: number;
  sma_period: number;
  ema_period: number;
  rsi_period: number;
  macd_fast: number;
  macd_slow: number;
  macd_signal: number;
  bollinger_period: number;
  bollinger_std: number;
}

export interface IndicatorPoint {
  time: string;
  sma: number | null;
  ema: number | null;
  bb_upper: number | null;
  bb_middle: number | null;
  bb_lower: number | null;
  rsi: number | null;
  macd_line: number | null;
  signal_line: number | null;
  histogram: number | null;
}

export interface FeeInfo {
  fee_pct: number;
}

// Normalized candle bar for lightweight-charts
export interface CandleBar {
  time: number;
  open: number;
  high: number;
  low: number;
  close: number;
}

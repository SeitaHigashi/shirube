# bitFlyer Lightning API 仕様書

> 参照元: https://lightning.bitflyer.com/docs  
> Realtime API: https://bf-lightning-api.readme.io/docs  
> Base URL: `https://api.bitflyer.com`

---

## 目次

1. [認証](#認証)
2. [HTTP Public API](#http-public-api)
3. [HTTP Private API](#http-private-api)
4. [Realtime API (WebSocket)](#realtime-api-websocket)
5. [レート制限](#レート制限)
6. [実装ファイル一覧](#実装ファイル一覧)

---

## 認証

Private API の呼び出しには以下の3つのHTTPヘッダーが必要。

| ヘッダー名         | 内容                         |
|--------------------|------------------------------|
| `ACCESS-KEY`       | API キー                     |
| `ACCESS-TIMESTAMP` | Unix タイムスタンプ（秒）    |
| `ACCESS-SIGN`      | HMAC-SHA256 署名（hex エンコード） |

**署名アルゴリズム:**

```
署名対象文字列 = timestamp + HTTP_METHOD + path_with_query + body
ACCESS-SIGN = HEX( HMAC-SHA256( api_secret, 署名対象文字列 ) )
```

- `HTTP_METHOD` は大文字（GET / POST / DELETE）
- `path_with_query` はクエリパラメータを含むパス（例: `/v1/me/getchildorders?product_code=BTC_JPY`）
- `body` は GET の場合は空文字列、POST/DELETE は JSON 文字列

---

## HTTP Public API

認証不要。全エンドポイントに `product_code` クエリパラメータ（例: `BTC_JPY`）が必要なものがある。

| メソッド | パス | 説明 | 実装済 |
|---------|------|------|--------|
| GET | `/v1/getmarkets` | 取引可能なマーケット一覧 | ✗ |
| GET | `/v1/getboard` | 板情報（オーダーブック） | ✗ |
| GET | `/v1/getticker` | ティッカー情報 | ✅ |
| GET | `/v1/getexecutions` | 最近の約定履歴 | ✗ |
| GET | `/v1/getboardstate` | 板の状態（運営状況） | ✗ |
| GET | `/v1/gethealth` | 取引所の稼働状況 | ✗ |
| GET | `/v1/getfundingrate` | CFD 商品のファンディングレート | ✗ |
| GET | `/v1/getcorporateleverage` | 法人向け最大レバレッジ | ✗ |
| GET | `/v1/getchats` | チャット履歴 | ✗ |

### GET /v1/getticker レスポンス（実装済）

```json
{
  "product_code": "BTC_JPY",
  "timestamp": "2024-01-01T00:00:00.000",
  "best_bid": 5000000,
  "best_ask": 5001000,
  "best_bid_size": 0.1,
  "best_ask_size": 0.2,
  "ltp": 5000500,
  "volume": 1234.5,
  "volume_by_product": 1000.0
}
```

### GET /v1/getboard レスポンス例

```json
{
  "mid_price": 33320,
  "bids": [{ "price": 30000, "size": 0.1 }],
  "asks": [{ "price": 36640, "size": 5.0 }]
}
```

---

## HTTP Private API

全エンドポイントに認証ヘッダーが必要。

### 口座・残高

| メソッド | パス | 説明 | 実装済 |
|---------|------|------|--------|
| GET | `/v1/me/getpermissions` | 使用可能な API 権限一覧 | ✗ |
| GET | `/v1/me/getbalance` | 保有資産残高一覧 | ✅ |
| GET | `/v1/me/getbalancehistory` | 残高変動履歴 | ✗ |
| GET | `/v1/me/getcollateral` | 証拠金状況 | ✗ |
| GET | `/v1/me/getcollateralaccounts` | 通貨別証拠金残高 | ✗ |
| GET | `/v1/me/getcollateralhistory` | 証拠金変動履歴 | ✗ |

### 入出金

| メソッド | パス | 説明 | 実装済 |
|---------|------|------|--------|
| GET | `/v1/me/getaddresses` | 暗号資産入金アドレス一覧 | ✗ |
| GET | `/v1/me/getcoinins` | 暗号資産入金履歴 | ✗ |
| GET | `/v1/me/getcoinouts` | 暗号資産出金履歴 | ✗ |
| GET | `/v1/me/getbankaccounts` | 登録銀行口座一覧 | ✗ |
| GET | `/v1/me/getdeposits` | 日本円入金履歴 | ✗ |
| POST | `/v1/me/withdraw` | 日本円出金 | ✗ |
| GET | `/v1/me/getwithdrawals` | 出金履歴 | ✗ |

### 注文

| メソッド | パス | 説明 | 実装済 |
|---------|------|------|--------|
| POST | `/v1/me/sendchildorder` | 注文送信（指値・成行） | ✅ |
| POST | `/v1/me/cancelchildorder` | 個別注文キャンセル | ✗ |
| POST | `/v1/me/sendparentorder` | 親注文送信（IFD/OCO等） | ✗ |
| POST | `/v1/me/cancelparentorder` | 親注文キャンセル | ✗ |
| POST | `/v1/me/cancelallchildorders` | 全注文一括キャンセル | ✅ |
| GET | `/v1/me/getchildorders` | 注文一覧 | ✅ |
| GET | `/v1/me/getparentorders` | 親注文一覧 | ✗ |
| GET | `/v1/me/getparentorder` | 親注文詳細 | ✗ |
| GET | `/v1/me/getexecutions` | 自分の約定履歴 | ✗ |
| GET | `/v1/me/getpositions` | 建玉一覧（FX） | ✅ |
| GET | `/v1/me/gettradingcommission` | 取引手数料率 | ✗ |

### POST /v1/me/sendchildorder リクエスト/レスポンス（実装済）

```json
// リクエスト
{
  "product_code": "BTC_JPY",
  "child_order_type": "LIMIT",   // "LIMIT" | "MARKET"
  "side": "BUY",                 // "BUY" | "SELL"
  "price": 5000000,              // LIMIT 注文時のみ
  "size": 0.01,
  "minute_to_expire": 43200,     // 有効期限（分）省略可
  "time_in_force": "GTC"         // "GTC" | "IOC" | "FOK" 省略可
}

// レスポンス
{ "child_order_acceptance_id": "JRF20240101-000000-000000" }
```

### GET /v1/me/getchildorders クエリパラメータ

| パラメータ | 必須 | 説明 |
|-----------|------|------|
| `product_code` | ✅ | 例: `BTC_JPY` |
| `child_order_state` | ✗ | `ACTIVE` / `COMPLETED` / `CANCELED` / `EXPIRED` |
| `count` | ✗ | 取得件数 |
| `before` | ✗ | このID以前の注文を取得 |
| `after` | ✗ | このID以降の注文を取得 |

### GET /v1/me/getbalance レスポンス（実装済）

```json
[
  { "currency_code": "JPY", "amount": 1000000, "available": 900000 },
  { "currency_code": "BTC", "amount": 0.5, "available": 0.4 }
]
```

### GET /v1/me/getpositions レスポンス（実装済）

```json
[
  {
    "product_code": "FX_BTC_JPY",
    "side": "BUY",
    "price": 5000000,
    "size": 0.1,
    "commission": 0,
    "sfd": 0
  }
]
```

---

## Realtime API (WebSocket)

**エンドポイント:** `wss://ws.lightstream.bitflyer.com/json-rpc`  
**プロトコル:** JSON-RPC 2.0

### サブスクリプション方法

```json
{
  "jsonrpc": "2.0",
  "method": "subscribe",
  "params": { "channel": "チャンネル名" },
  "id": null
}
```

### Public チャンネル（認証不要）

| チャンネル名 | 説明 | 実装済 |
|-------------|------|--------|
| `lightning_board_snapshot_{product_code}` | 板情報スナップショット | ✗ |
| `lightning_board_{product_code}` | 板情報差分 | ✗ |
| `lightning_ticker_{product_code}` | ティッカー更新 | ✅ |
| `lightning_executions_{product_code}` | 約定履歴ストリーム | ✅ |

### Private チャンネル（認証必要）

| チャンネル名 | 説明 | 実装済 |
|-------------|------|--------|
| `child_order_events` | 注文イベント（注文・約定・キャンセル等） | ✗ |
| `parent_order_events` | 親注文イベント | ✗ |

### lightning_ticker_{product_code} メッセージ（実装済）

```json
{
  "product_code": "BTC_JPY",
  "timestamp": "2024-01-01T00:00:00.000",
  "best_bid": 5000000,
  "best_ask": 5001000,
  "best_bid_size": 0.1,
  "best_ask_size": 0.2,
  "ltp": 5000500,
  "volume": 1234.5,
  "volume_by_product": 1000.0
}
```

### lightning_executions_{product_code} メッセージ（実装済）

```json
[
  {
    "id": 123456,
    "exec_date": "2024-01-01T00:00:00.000",
    "price": 5000000,
    "size": 0.01,
    "side": "BUY",
    "buy_child_order_acceptance_id": "JRF20240101-000000-000001",
    "sell_child_order_acceptance_id": "JRF20240101-000000-000002"
  }
]
```

### child_order_events メッセージ

```json
{
  "product_code": "BTC_JPY",
  "child_order_id": "JOR20240101-000000-000000",
  "child_order_acceptance_id": "JRF20240101-000000-000000",
  "event_date": "2024-01-01T00:00:00.000",
  "event_type": "EXECUTION",
  "side": "BUY",
  "price": 5000000,
  "size": 0.01,
  "exec_id": 123456,
  "commission": 0
}
```

`event_type` の種類: `ORDER` / `ORDER_FAILED` / `CANCEL` / `CANCEL_FAILED` / `EXECUTION` / `EXPIRE`

### lightning_board_snapshot_{product_code} メッセージ

```json
{
  "mid_price": 5000500,
  "bids": [{ "price": 5000000, "size": 0.1 }],
  "asks": [{ "price": 5001000, "size": 0.2 }]
}
```

---

## レート制限

| 項目 | 値 |
|------|-----|
| 上限 | 200 リクエスト / 分 |
| アルゴリズム | トークンバケット |
| 補充レート | 約 3.33 tokens/秒 |

現在の実装では全エンドポイント（Public / Private 共通）に同一のレート制限を適用している。

---

## 実装ファイル一覧

| ファイル | 内容 |
|---------|------|
| `src/exchange/bitflyer/auth.rs` | HMAC-SHA256 署名生成 |
| `src/exchange/bitflyer/models.rs` | API レスポンス型定義 |
| `src/exchange/bitflyer/rest.rs` | REST クライアント実装 |
| `src/exchange/bitflyer/ws.rs` | WebSocket クライアント・再接続ロジック |
| `src/exchange/rate_limiter.rs` | トークンバケット実装 |
| `src/exchange/mock.rs` | MockExchangeClient（テスト用） |
| `src/exchange/mod.rs` | ExchangeClient トレイト定義 |

# SoraHost ポータル連携ガイド

SoraHost（`dev-sorahost`、Laravel）から suhms を操作するための仕様。ポータル側の実装はこのリポジトリには含めない。
PHP クライアントの例は [`examples/SuhmsAdminClient.php`](examples/SuhmsAdminClient.php)。

## 前提

- 管理 API は `[admin] listen` で待ち受ける（例: `http://10.0.0.1:9200`）。**プライベートネットワークからのみ到達可能にすること。**
- 認証: `Authorization: Bearer <token>`（`[admin] token_file` の内容、32 文字以上）。Laravel 側では `.env` に
  `SUHMS_ADMIN_URL` / `SUHMS_ADMIN_TOKEN` を置く想定。
- リクエスト/レスポンスは JSON。エラーは `{"error": "..."}` と HTTP ステータス（400 / 401 / 403 / 404 / 409 / 500）。
- 時刻はすべて Unix ミリ秒。容量はバイト（論理バイト = オブジェクトサイズの合計。レプリカ分は含まない）。

## ポータルの業務フローと API

| 業務 | API |
|---|---|
| 申込み（S3 プラン契約） | `POST /admin/v1/tenants` → `POST /admin/v1/tenants/{id}/keys` |
| プラン変更 | `PATCH /admin/v1/tenants/{id}` で `quota_bytes` / `max_buckets` |
| 未払いによる停止 / 再開 | `PATCH /admin/v1/tenants/{id}` で `{"suspended": true/false}` |
| アクセスキーの追加発行 | `POST /admin/v1/tenants/{id}/keys`（シークレットはこのレスポンスでのみ返る） |
| アクセスキーの無効化 | `PATCH /admin/v1/keys/{access_key_id}` で `{"active": false}` |
| 契約状況の表示 | `GET /admin/v1/tenants/{id}`（現在の使用量つき）、`GET /admin/v1/tenants/{id}/buckets` |
| 冗長度の変更（プラン差別化） | `PATCH /admin/v1/buckets/{name}` で `{"replicas": 1|2|3|null}` |
| 公開バケット（Web 配信） | `PATCH /admin/v1/buckets/{name}` で `{"public_read": true}`（サーバ設定で許可が必要） |
| 月次請求 | `GET /admin/v1/tenants/{id}/usage?from=..&to=..` |
| 監視 | `GET /admin/v1/cluster`、`GET /metrics`（Prometheus） |

テナント ID（UUID）は suhms が採番する。ポータル側の顧客テーブルに `suhms_tenant_id` として保存する。
テナント名は一意で、SoraHost の顧客番号などを入れる。

### テナント作成

```http
POST /admin/v1/tenants
{"name": "cust-000123", "quota_bytes": 107374182400, "max_buckets": 100}
```
```json
201 {"id":"81add317-...","name":"cust-000123","created_at":1794596846043,
     "suspended":false,"quota_bytes":107374182400,"max_buckets":100}
```
同名があれば 409。

### アクセスキー発行

```http
POST /admin/v1/tenants/{id}/keys
```
```json
201 {"access_key_id":"SUI3YONYA199YBXPC0EC","secret_access_key":"..."}
```
**シークレットはこの一度しか取得できない。** 顧客画面で一度だけ表示し、ポータル側の DB には保存しない
（保存するなら Laravel の encrypted cast で暗号化すること）。

### 設定変更（PATCH の意味論）

フィールドを省略すると変更なし、`null` は「上限なし」。

```http
PATCH /admin/v1/tenants/{id}
{"quota_bytes": null, "suspended": false}
```

停止・キー無効化は、変更を受けたノードでは即時、クラスタの他ノードでも 10 秒以内に反映される。
停止中の S3 リクエストは `403 AccessDenied`（"This account is suspended"）。データは削除されない。

## 課金（Stripe 従量課金への対応）

`GET /admin/v1/tenants/{id}/usage?from=<ms>&to=<ms>` は時間単位の行と合計を返す。

```json
{
  "hours": [{"hour":1794592800000,"class_a_requests":34,"class_b_requests":31,
             "ingress_bytes":91,"egress_bytes":0,"stored_objects":31,
             "logical_bytes":91,"stored_bytes":273}, ...],
  "logical_byte_hours": 327600,
  "class_a_requests": 34, "class_b_requests": 31,
  "ingress_bytes": 91, "egress_bytes": 0
}
```

| 請求項目 | 計算 |
|---|---|
| 保存容量（GB-月） | `logical_byte_hours / 1e9 / その月の時間数`（容量スナップショットは leader が 1 時間ごとに記録） |
| リクエスト（class A: PUT/COPY/POST/LIST） | `class_a_requests` |
| リクエスト（class B: GET/HEAD 他） | `class_b_requests` |
| 転送量（ダウンロード） | `egress_bytes`（公開バケットへの匿名アクセス分はバケット所有者に計上） |

- `stored_bytes` はレプリカ込みの実ディスク使用量。冗長度でプラン単価を変える場合に使う。
- 推奨: 月初に前月分を取得し、Cashier の `reportUsage`（メーター付き価格）に送る。`from` は月初 0 時（UTC かどうかを
  請求ポリシーで決めること）、`to` は翌月初。
- メーターはノードごとに最大 60 秒（`[maintenance] usage_flush_secs`）遅れて反映される。usage API は呼ばれたノードの未反映分を
  書き出してから集計するが、他ノードの直近 60 秒分は含まれないことがある。前月分の確定集計なら問題にならない。

## エラー時の扱い

- 5xx はリトライしてよい（テナント作成は 409 が返れば作成済み）。
- キー発行は冪等ではない。タイムアウトした場合は `GET /admin/v1/tenants/{id}/keys` で確認し、不要なキーは無効化する。

## 顧客向けの接続情報（ポータルに表示する内容）

| 項目 | 値 |
|---|---|
| エンドポイント | `https://s3.sorahost.net`（nginx でTLS終端。[`../deploy/nginx.conf`](../deploy/nginx.conf)） |
| リージョン | `jp-1`（`[s3] region`） |
| アドレス形式 | パス形式（`https://s3.sorahost.net/<bucket>/<key>`）。仮想ホスト形式を使うなら `[s3] domains` とワイルドカード証明書 |
| ストレージクラス | `STANDARD`（3 重）/ `STANDARD_IA`（2 重）/ `REDUCED_REDUNDANCY`（1 重。ディスク故障で消える旨を利用規約に明記） |

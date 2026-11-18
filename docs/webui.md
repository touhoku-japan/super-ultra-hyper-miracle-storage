# Web コンソール

MinIO Console のように、ブラウザで操作・設定できる画面。2026-11-15 に計画し、同日に実装した（フェーズ 0〜4）。
利用者は**顧客と運用者の両方**。1 つの Svelte アプリを 2 か所で配信し、ログインの種類で画面を切り替える。

| | 顧客用コンソール | 運用者用コンソール |
|---|---|---|
| 配信場所 | `[console] listen`（新規リスナー）の `/`。nginx で専用ホスト名（例: `console.s3.sorahost.net`）に公開 | 管理 API と同じ `[admin] listen` の `/ui/`。社内網だけ |
| ログイン | アクセスキー ID ＋ シークレット（S3 クライアントと同じ） | 管理トークン（`SUHMS_ADMIN_TOKEN` / `[admin] token_file`） |
| 範囲 | 自分のテナントだけ | 全テナントとクラスタ |
| 主な画面 | バケット一覧、ファイルブラウザ、バケット設定、使用量 | 概要、テナント、ディスクとノード、メンテナンス |

申込・課金・アクセスキーの初回発行は従来どおり SoraHost ポータル（dev-sorahost）の役割。このリポジトリでは実装しない。

## 構成

- フロントエンド: `web/`（Svelte 5 + Vite。ルーティングはハッシュ形式 `#/...`）。`npm run build` の出力 `web/dist/` を
  `crates/server/build.rs` がバイナリに埋め込む（`crates/server/src/web.rs` が配信）。`web/dist/` が無いと案内ページだけになるので、
  `cargo build` や `cargo test` に Node.js は要らない。`SUHMS_REQUIRE_WEB=1` を付けると、`web/dist/` が無いときにビルドを失敗させる（CI ではこれを付けている）
- 顧客用バックエンド: `crates/server/src/console.rs`
  - `GET api/session` / `POST api/login` / `POST api/logout`
  - `GET api/buckets`（S3 に API が無い「公開」「既定のコピー数」を含む）、`GET api/usage`（保存量・今月のメーター）
  - `POST api/presign`（共有リンク。`[console] s3_public_url` を設定したときだけ）
  - `/s3/<bucket>/<key>?<query>`: **S3 プロキシ**。ブラウザは普通の S3 リクエストを送り、サーバがセッションのアクセスキーで
    SigV4 署名（`UNSIGNED-PAYLOAD`。`crates/server/src/sigv4.rs`）して S3 サービスをプロセス内で呼ぶ。このためテナントの範囲、停止、
    レート制限、課金メーター、監査ログは SDK から使った場合とまったく同じになる。コンソール専用のデータ操作 API は作っていない
- 運用者用バックエンド: `crates/server/src/admin_api.rs`
  - `/ui/api/session|login|logout`、`/ui/*` で静的ファイル
  - 管理 API は Bearer トークンに加えて運用者のセッション cookie も受け付ける（cookie の場合は書き込みに CSRF ヘッダが必要）
  - 追加したエンドポイント: `GET /admin/v1/overview`、`GET /admin/v1/storage`（テナントごとの保存量）、
    `GET /admin/v1/disks`、`POST /admin/v1/disks/{id}/drain|undrain`、`DELETE /admin/v1/disks/{id}?force=true`、
    `POST /admin/v1/nodes/{id}/drain|undrain`、`DELETE /admin/v1/nodes/{id}?force=true`、
    `GET /admin/v1/jobs`、`POST /admin/v1/jobs/{scrub|repair|repair-full|gc|rebalance|checksum-backfill}`（JSON で引数を渡せる。
    `crates/server/src/jobs.rs`）
  - ジョブは管理 API を受けたノードのプロセス内でバックグラウンド実行する。scrub と gc はそのノードのディスクだけが対象。
    repair / rebalance / checksum-backfill はリースを取るので、他ノードが実行中なら「実行中」として失敗する
  - 実行状態はメタデータ DB の `job_runs`（ノード × ジョブごとに最後の 1 回）に記録し、どのノードの管理 API からも全ノード分が見える。
    コンソールから開始したものだけでなく `[maintenance]` の定期実行も記録する（`started_by` が `console` / `schedule`）。
    定期実行の repair 等が他ノードのリースに当たって何もしなかった場合は記録を前の状態に戻す（毎回「スキップ」が並ばないように）。
    実行中は 30 秒ごとに `heartbeat_at` を更新し、2 分途絶えた行は「中断（ノードからの応答なし）」として表示する。
    起動時にそのノードの「実行中」のまま残った行を「再起動で中断」にする
  - `GET /admin/v1/jobs` はジョブごとに `scope`（`node` / `cluster`）、`blocked`（今このノードから開始すると拒否される）、
    `runs`（ノードごとの最後の実行。`node_id` / `this_node` / `running` / `interrupted` / `started_by` / `started_at` / `finished_at` /
    `result` / `error`）を返す。クラスタで 1 台のジョブは、どこかのノードで実行中なら開始を 409 で拒否する

## セキュリティ上の判断

- **顧客用コンソールは S3 と同じホスト名に置かない。** 公開バケットは S3 のホスト名で顧客の HTML をそのまま配信するので、
  同じオリジンにコンソールがあるとそのページからセッションを使われる。cookie には Domain 属性を付けない（ホスト限定）。
  ブラウザは cookie をポートで区別しないので、**同じホスト名でポートだけ変えるのも不可**（S3 側のリクエストにもコンソールの cookie が付く）
- **シークレットはブラウザに渡さない。** ログイン時にサーバで照合し、その後はセッション cookie だけを使う
  - セッション cookie: `HttpOnly; SameSite=Strict`（顧客用は既定で `Secure`）、HMAC-SHA256 で署名した `{sub, exp}`。サーバ側には何も保存しない
  - 署名鍵: 顧客用はマスターキーから導出（全ノード共通なので、どのノードでも検証できる）、運用者用は管理トークンから導出（トークンを変えると全セッションが無効になる）
  - 毎リクエスト、アクセスキーが有効か・テナントが停止していないかを `Directory`（10 秒キャッシュ）で確認する。キーの無効化やテナント停止は 10 秒以内に効く
  - 有効期限: 顧客 12 時間（`[console] session_secs`）、運用者 8 時間（`[admin] ui_session_secs`）
- CSRF: GET / HEAD 以外は `x-suhms-console` ヘッダが必須（クロスサイトのフォームからは付けられない）。ログイン自体も対象（ログイン CSRF 対策）
- ログイン試行の制限: 失敗が IP ごと・アクセスキーごとに 5 分で 10 回を超えると 429。nginx 経由なら `[console] trust_proxy = true` で
  `X-Forwarded-For` の最後の値を IP とする
- S3 プロキシの応答は常に `Content-Disposition: attachment`（`response-content-disposition` で attachment を指定した場合はそのファイル名を残す）、
  `Content-Security-Policy: sandbox; default-src 'none'`、`nosniff`。顧客のファイルがコンソールのオリジンで表示・実行されることはない
- コンソールのページ: `default-src 'self'`（インラインスクリプト不可）、`frame-ancestors 'none'`、`X-Frame-Options: DENY`、`Referrer-Policy: no-referrer`
- プロキシが S3 に渡すリクエストヘッダは許可リスト（`content-*`、`range`、`if-*`、`x-amz-*` など）だけ。cookie は渡さない
- 監査ログ（`suhms::audit`）: ログイン成功・失敗・制限、ログアウト、共有リンク発行、運用者の管理 API 操作、ジョブの開始。
  S3 プロキシ経由の操作は従来の S3 監査ログにアクセスキー付きで出る

## 計画からの変更点

- **ダウンロードは署名付き URL ではなくプロキシ経由にした。** 計画では S3 のエンドポイントへ署名付き URL で直接取りに行く案だったが、
  プロキシはプロセス内で S3 サービスを呼ぶだけなので負荷はほぼ同じで、CORS の設定も、`s3_public_url` の設定も要らない。署名付き URL は
  「共有リンク」（ログインしていない人に渡す期限付き URL、最長 7 日）にだけ使う
- 顧客用のデータ操作 API を `Store` の上に新しく作らず、S3 プロキシにした（上記）。テナント境界や課金の実装を二重に持たないため
- ポータルからのワンタイムログイン（ログイン済みのままコンソールへ移る）は未実装。セッションは `{sub, exp}` の署名 cookie なので、
  管理 API で短命のトークンを発行して `api/login` で交換する形で後から足せる

## 画面と機能

顧客用:
- バケット: 一覧・作成（バージョニング / オブジェクトロックの指定）・削除（名前の入力で確認）
- ファイルブラウザ: フォルダ（プレフィックス）単位の表示、絞り込み、「さらに読み込む」によるページング、ファイル・フォルダのアップロード
  （ドラッグ＆ドロップ可。64 MiB 以上はマルチパートで並列 3 本、進捗表示・中止。保存先ストレージクラスを選べる）、ダウンロード、
  複数選択しての削除、フォルダごとの削除、フォルダの作成、全バージョンの表示と版の削除
- オブジェクトの詳細: プロパティ（チェックサムを含む）、ユーザーメタデータ、タグの編集、バージョン一覧（ダウンロード・この版に戻す・削除）、
  共有リンク、オブジェクトロック（保持期間・リーガルホールド）
- バケットの設定: バージョニング、オブジェクトロックの既定保持、ライフサイクル（プレフィックス・タグ・サイズの条件、期限・日付、
  古い版の削除と残す数、削除マーカーの掃除、ストレージクラスの移行、未完了アップロードの中止）、CORS、未完了のアップロードの一覧と中止。
  公開と既定のコピー数は表示のみ（変更は運用者）
- 使用量: 保存量と容量上限、オブジェクト数、バケット数、今月のリクエスト数と通信量

運用者用:
- 概要: ディスクの稼働数、repair 待ちの件数（1 万件以上は「+」）、保存量、テナント数。停止中のディスクがあると警告
- テナント: 一覧（保存量つき）・作成、詳細（容量上限・バケット数上限、停止 / 解除、アクセスキーの発行（シークレットは一度だけ表示）と
  有効 / 無効、バケットの既定コピー数と公開設定、月ごとの利用量）
- ディスクとノード: ノードごとのディスク一覧（稼働・撤去中・空き容量・コピー数・最終応答）、ディスク / ノードの撤去（drain）と取り消し、
  登録の削除（コピーが残っているときは強制オプション）
- メンテナンス: scrub / repair / repair（全件）/ gc / rebalance / チェックサムの後付けを今すぐ実行。各ジョブについて、
  全ノードの最後の実行（定期実行を含む。実行中・完了・失敗・中断、所要時間、結果）を表示

## 運用

1. `cd web && npm ci && npm run build` のあと `cargo build --release -p suhms-server`（CI の `check` ジョブも同じ順）
2. 設定（`config.example.toml` 参照）:
   ```toml
   [admin]
   listen = "10.0.0.1:9200"      # 運用コンソールは http://10.0.0.1:9200/ui/
   [console]
   listen = "127.0.0.1:9300"
   s3_public_url = "https://s3.sorahost.net"
   trust_proxy = true
   ```
3. nginx に顧客用コンソールのホスト名を追加（`deploy/nginx.conf` の 2 つ目の server ブロック）。管理 API は従来どおり公開しない。
   運用コンソールは社内網か SSH トンネル（`ssh -L 9200:10.0.0.1:9200 ...` で `http://localhost:9200/ui/`）で開く
4. 開発: `SUHMS_CONSOLE=http://127.0.0.1:9300 npm --prefix web run dev`（顧客用）または `SUHMS_ADMIN=http://127.0.0.1:9200 npm --prefix web run dev`
   （運用者用。`http://localhost:5173/ui/` ではなく `http://localhost:5173/` で開き、API だけをプロキシする）。
   ローカルの HTTP で顧客用コンソールを試すときは `[console] secure_cookie = false`

## テスト

- Rust: プロキシ・セッション・CSRF・テナント間アクセス・ジョブ等を `crates/server/tests/console.rs` で（ルータを in-process で叩く）
- フロントエンドの純粋な関数（XML 生成・URL エンコード等）: `npm test`（`web/src/lib/s3xml.test.js`、Node の test runner）
- ブラウザ E2E: `web/e2e/`（Playwright + Chromium）。`e2e/server.js` が一時ディレクトリにディスク 3 本・SQLite の設定を作り、
  空いているポートで本物の `suhms serve`（S3・管理 API・顧客用コンソール）を起動して、顧客用と運用者用の両方を操作する。
  対象: ログイン（失敗時の表示）、バケットの作成・削除、日本語名のアップロード、65 MiB のマルチパート（読み戻して SHA-256 を照合）、
  タグ、共有リンク（cookie なしで取得）、ダウンロード、フォルダ、バケット数の上限、運用者によるテナント作成・キー発行・停止・
  キーの無効化（開いている顧客セッションへの反映）、ディスク一覧、ジョブの実行、HTML オブジェクトがダウンロードになり
  スクリプトが動かないこと、CSRF ヘッダなしの拒否、cookie の属性と `frame-ancestors`
  ```sh
  cd web && npm run build && cd .. && cargo build -p suhms-server   # コンソールを埋め込んだバイナリ
  cd web && npx playwright install chromium-headless-shell          # 初回のみ（CI は --with-deps も付ける）
  npm run e2e                                                        # SUHMS_BIN で別のバイナリ、SUHMS_E2E_KEEP=1 で一時ディレクトリを残す
  ```
  AlmaLinux 10 は Playwright の公式対応 OS ではないが、ubuntu24.04 向けのビルドが動く。必要なシステムライブラリはホストの
  構成で変わるので、`ldd <chrome-headless-shell> | grep 'not found'` で足りないものを確かめて入れる（開発ホストで入れたものは
  `CLAUDE.md` の「開発ホスト」）。CI では `e2e` ジョブが実行し、失敗時はレポートとトレースを artifact に残す

## 既知の制限

- ログアウトは cookie を消すだけ。漏れた cookie は有効期限まで使える（キーの無効化かテナントの停止で即座に止められる。運用者用は管理トークンの変更で全セッションが無効になる）
- ジョブの記録はノードごとに最後の 1 回だけ（履歴は監査ログ・通常ログに残る）。コンソールからの開始はそのノードでだけ行える
  （別ノードで実行したいときはそのノードの `/ui/` を開く）
- 顧客の「フォルダごとの削除」と複数削除は、バージョニングが有効なバケットでは削除マーカーを付けるだけ（以前の版は残る）
- 運用者用コンソールから顧客のオブジェクトの中身は見られない（意図的。必要ならそのテナントのキーで顧客用コンソールにログインする）
- ビルド時の依存: Vite 8 が CSS の圧縮に使う `lightningcss` は MPL-2.0（ファイル単位の弱いコピーレフト）。ビルドツールとして使うだけで、
  バイナリにもコンソールの配信物にも含まれない。Svelte のランタイム（バンドルに入る）は MIT

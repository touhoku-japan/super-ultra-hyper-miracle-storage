# CLAUDE.md

S3 互換オブジェクトストレージの Rust 実装（旧 Laravel スケルトンからの置き換え）。
SoraHost の社内バックアップ＋顧客向け外販が目的。マルチテナントと、複数ディスク/複数ノードへのレプリケーション（ファイルを丸ごとコピー）による冗長化が必須要件。

## コマンド

- テスト: `CARGO_BUILD_JOBS=1 cargo test --workspace`
- Lint: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
- 起動: `cargo run -p suhms-server -- -c config.toml serve`（`GET /suhms/health` で疎通確認。認証不要）
- 管理: `suhms gen-master-key` / `suhms tenant create|list` / `suhms key create|list|disable|enable` /
  `suhms bucket set-replicas <bucket> <n|default>` / `suhms scrub` / `suhms repair`
  - tenant/key 系はメタデータ DB だけを触るので、サーバ稼働中に実行してよい
  - ディスクの一時ファイル掃除（`Store::clear_tmp`）は `serve` 起動時のみ。管理コマンドは掃除しない（稼働中サーバの書き込み中ファイルを消さないため）

開発ホストは 2 コア / 2.9GB RAM で他アプリと共有しているため、ビルドは `CARGO_BUILD_JOBS=1` で行う。
release ビルドは CI 側で行う想定。aws-sdk-s3 のような巨大クレートはこのホストでのビルドが重いので dev-dependency に入れない。
実クライアントでの互換確認はホストにある `rclone` で行う（環境変数 `RCLONE_CONFIG_S_*` で設定、`force_path_style=true`）。

## 構成

- Cargo workspace。クレートは `crates/*`
  - `server/` — lib + バイナリ `suhms`
    - `lib.rs` — `app()`: axum ルータ（ヘルスチェック + それ以外は s3s の S3 サービス）。外側から CORS → 監査ログ → TraceLayer の順にレイヤーを重ねる
    - `s3.rs` — `s3s::S3` 実装。リクエストごとに検証済みアクセスキー → テナント ID を引き、store にテナントを渡す。
      `KeyStore`（`S3Auth`）が SigV4 用にシークレットを復号して返す。body 読み取り中の s3s エラー（SHA-256 不一致等）は `body_error` で S3 エラーに戻す
    - `crypto.rs` — アクセスキーのシークレットを AES-256-GCM で暗号化（AAD = アクセスキー ID）。マスターキーは `SUHMS_MASTER_KEY` か `[auth] master_key_file`
    - `checksum.rs` — `x-amz-checksum-*`（ヘッダ、または aws-chunked のトレーラ）を body を流しながら検証・計算する reader。
      不一致は EOF で読み取りエラーにし、store がコミット前に書き込みを捨てる。計算結果は `ChecksumSlot` 経由で
      store に渡り、objects / multipart_parts の `checksum` 列に JSON（algorithm / type / value）で保存。
      GET/HEAD は `x-amz-checksum-mode: ENABLED` のときだけ返す（Range 付きは返さない）。
      `checksum::Policy` で保存するアルゴリズムを決める: 指定なしのアップロードは S3 と同じく CRC64NVME（`DEFAULT_ALGO`）を
      付け、クライアントが別アルゴリズムを送ればそれも同時に計算して検証する。アルゴリズム指定なしのマルチパートは
      パートに CRC64NVME を持たせ、完了時に FULL_OBJECT の CRC64NVME を合成する。対応アルゴリズムは CRC32 / CRC32C /
      CRC64NVME / SHA1 / SHA256 / SHA512 / MD5 / XXHASH64 / XXHASH3 / XXHASH128（FULL_OBJECT は CRC 系のみ）。
      マルチパートは作成時にアルゴリズムと型を決め（upload の `checksum` 列）、各パートはサーバ側でも計算する。
      完了時に COMPOSITE（パートのチェックサムを連結してハッシュ + `-N`）か FULL_OBJECT（CRC をパートから
      `crc_fast::checksum_combine` で合成。データは読み直さない）を求める。コピーは要求されたアルゴリズム、
      無ければコピー元のアルゴリズムで計算し直す
    - `cluster.rs` — ノード間 API（`[cluster] listen` の別リスナー、`x-suhms-node-auth` の HMAC 認証）と
      `HttpPeer`（`reqwest`）。`open_store` がクラスタ構成ならピア付きで Store を開く
    - `tls.rs` — ノード間の相互 TLS（`[cluster.tls]`）。rustls（暗号プロバイダは `ring`。aws-lc はビルドが重いので使わない）。
      クラスタ CA が署名したクライアント証明書を必須にし、サーバ証明書も同じ CA でだけ検証する。HMAC 認証は残す。
      `TlsListener` はハンドシェイクを別タスクで行う `axum::serve` 用リスナー。手順は `docs/cluster-tls.md`
    - `tenancy.rs` — アクセスキー/テナントの 10 秒キャッシュ（`Directory`。管理 API での変更は同ノードで即時無効化）、
      アクセスキーごとのレート制限（トークンバケット）、課金用メーター（`Meter`、class A/B リクエスト数・ingress/egress を
      テナント×時間で集計し定期的に `usage_hourly` へ加算）、Prometheus メトリクス
    - `admin_api.rs` — SoraHost ポータル用の管理 API（`[admin] listen`、Bearer トークン）。`/metrics` だけ認証なし
    - `admin.rs` — 管理サブコマンド
    - `tests/` — `common/mod.rs` の自前 SigV4 署名器で、実ルータを in-process で叩く E2E テスト
  - `store/` — ストレージエンジン
    - `disk.rs` — ディスク = 既存ディレクトリ。`.suhms/format.json` の UUID で識別（マウント順が変わっても同一視）。
      ルートが無ければ作らない（未マウント時にルート FS へ書くのを防ぐ）。中身のある未フォーマットのディレクトリは使わない
    - `placement.rs` — 配置（別ノード優先 → 同ノード別ディスク、空き容量の多い順）と ACK 閾値（過半数）
    - `blob.rs` — 全レプリカへの同時ストリーミング書き込み（MD5=ETag と BLAKE3 を同時計算）、検証付きコピー
    - `cluster.rs` — `Peer`（他ノードのディスク操作）/ `Peers` / `ReplicaSink` / `DiskRef`（ローカルかリモートかを隠す）
    - `meta/` — `MetadataStore` trait と SQLite / PostgreSQL / MariaDB(MySQL) 実装。スキーマは `meta/migrations/<backend>/NNNN_*.sql`
      （リリース済みのマイグレーションは編集しない。バックエンドごとに番号は独立）。
      `postgres.rs` / `mysql.rs` は `sqlite.rs` と同じ構造（SQL の差分: プレースホルダ `$n`、`ON DUPLICATE KEY UPDATE`、
      MySQL の object_key は `VARBINARY(1024)`、PG は `COLLATE "C"`）。SQLite 以外は書き込み競合を `is_retryable` で再試行
    - クラスタ: 各ノードが自ディスクの空き容量と heartbeat を `disks` に書く（`HEARTBEAT_INTERVAL`）。30 秒更新がないディスクは offline 扱い。
      `Store::cluster_disks()` が全ノードのディスク一覧（2 秒キャッシュ）。レプリカ上限は「登録済み」ディスク/ノード数で判定するので、
      ノード障害中も書き込みは過半数で成功し、復旧後に repair で補う。複数ノード構成では同一ノードに 2 コピー置かない
    - `list.rs` — S3 の ListObjects 意味論（prefix / delimiter / marker / continuation）。キー順は UTF-8 バイト順
    - `store.rs` — `Store`: テナント・バケット・オブジェクト操作（すべて owner = テナント ID でスコープ。他テナントのバケットは AccessDenied）、
      コピー、マルチパート、`scrub()`、`repair()`
    - マルチパート: アップロード開始時にディスクを決め、パートは各ディスクの `.suhms/multipart/<upload>/<blob>` に置く。
      完了時は各ディスク上でローカルにパートを連結して普通のオブジェクトファイルにする（パートごとに BLAKE3 を検証）。
      ETag は S3 と同じ `md5(パートMD5の連結)-N`。完了処理中は s3s がキープアライブの空白を送る
- S3 プロトコル層は `s3s` クレート（Apache-2.0。`openssl` feature は使わない）
- 商用機能: テナント停止（全 S3 リクエスト拒否）、容量クォータ（論理バイト。30 秒キャッシュなので少し超過しうる）、
  バケット数上限、公開バケット（`[s3] allow_public_buckets` + 管理 API でバケットごとに許可。匿名は GET/HEAD のみ、
  通信量はバケット所有者に課金）、監査ログ（`suhms::audit` ターゲット。ログレベルに関係なく出力）、
  定期メンテナンス（`[maintenance]`: 全ノードで heartbeat・メーター書き出し・scrub、leader だけ repair・放置マルチパート削除・容量スナップショット）
- バージョニング: 全バージョンを `objects` テーブルに持つ（`version_id` / `is_latest` / `delete_marker`）。最新版の一意性は
  部分ユニークインデックス（SQLite/PG）、MariaDB は生成列 `latest_key` で担保。`MetadataStore::put_object` が
  `VersioningMode`（Unversioned / Enabled / Suspended）に従って旧版を削除・非現行化し、削除した版を返す（呼び出し側がファイル削除）。
  未バージョンのオブジェクトは `version_id = "null"`
- Object Lock（`store/src/lock.rs`）: バケット作成時の `x-amz-bucket-object-lock-enabled`、既定保持期間、オブジェクト単位の
  retention（GOVERNANCE はバイパスヘッダで解除可、COMPLIANCE は短縮・削除不可）と legal hold。保護対象は「版の削除」で、
  キーの DELETE（削除マーカー追加）は可能
- ライフサイクル（`store/src/lifecycle.rs`）: 期限切れ（現行版）、非現行版の削除、ExpiredObjectDeleteMarker
  （他の版が無い削除マーカーの削除）、transition / noncurrent transition、未完了マルチパートの中止。prefix 単位。
  leader が 1 時間ごとに `Store::apply_lifecycle`。transition は「ストレージクラス＝レプリカ数」の変更
  （`set_object_storage_class` で desired_replicas を変え、repair がコピーを増減）。複数の transition は日数が最大の
  ものだけを適用。設定時に未知のストレージクラスは InvalidStorageClass。期限・移行は日数か Date（その日時以降は全対象）、
  フィルタは prefix と ObjectSizeGreaterThan / LessThan（`And` 可）、NewerNoncurrentVersions（キーごとに新しい非現行版を
  N 個残す）に対応。タグでのフィルタはオブジェクトタグ自体が無いので NotImplemented
- 書き込みの前提条件（`meta::WriteCondition`）: PUT / CompleteMultipartUpload の `If-None-Match: *` と `If-Match`
  （ETag か `*`）。メタデータのトランザクション内で最新版に対して判定（無ければ NoSuchKey、不一致は 412）
- versionId 付きのコピー元（CopyObject / UploadPartCopy）。応答に `x-amz-copy-source-version-id`。
  削除マーカーは MethodNotAllowed。同じキーへの旧版コピー（復元）は可
- ListObjectVersions / ListMultipartUploads の delimiter（CommonPrefixes）。store 側で `list.rs` と同じく
  共通プレフィックスの後ろ（`PREFIX_SKIP`）へカーソルを飛ばして読む。key-marker だけ指定した ListMultipartUploads は
  そのキーのアップロードを含めない（S3 と同じ）
- 予約バケット名 `suhms`（`RESERVED_BUCKETS`）: ヘルスチェック `/suhms/health` 用。パス形式のときだけヘルスチェックとして扱い、
  仮想ホスト形式（`<bucket>.<domain>/suhms/health`）はそのバケットのオブジェクトへのリクエストとして S3 に回す
- CORS（`server/src/cors.rs`）: ルールはバケットに JSON で保存し、ミドルウェアがプリフライト（OPTIONS）と応答ヘッダを処理
- SQLite のマイグレーションは外部キーを無効化して実行する（`objects` の再作成で replicas が CASCADE 削除されないように）
- サーバ側暗号化（SSE）は実装しない: 「ファイルをそのまま置く」方針と矛盾するため。保存時暗号化はディスク側（LUKS 等）で行う
- 対応済み S3 操作: ListBuckets, Create/Head/Delete Bucket, GetBucketLocation, GetBucketVersioning,
  Put/Get（Range）/Head/Delete Object, DeleteObjects, ListObjects V1/V2（encoding-type=url 対応）, presigned URL,
  CopyObject（COPY/REPLACE）, Create/UploadPart/UploadPartCopy/Complete/Abort MultipartUpload, ListParts, ListMultipartUploads,
  条件付きリクエスト（GET/HEAD の If-Match / If-None-Match / If-(Un)Modified-Since、PUT / CompleteMultipartUpload の
  If-Match / If-None-Match: *）, チェックサム（CRC32 / CRC32C / CRC64NVME / SHA1 / SHA256 / SHA512 / MD5 / xxHash 系、
  マルチパートの COMPOSITE / FULL_OBJECT、未指定時の CRC64NVME）, versionId 付きコピー元, ListObjectVersions / ListMultipartUploads の delimiter,
  Put/GetBucketVersioning, ListObjectVersions, versionId 付き GET/HEAD/DELETE, Put/Get/DeleteBucketLifecycleConfiguration,
  Put/GetObjectLockConfiguration, Put/GetObjectRetention, Put/GetObjectLegalHold, Put/Get/DeleteBucketCors

## ドキュメント・デプロイ

- `docs/TODO.md` — やり残し（バックログ）。項目を片付けたらここと下の「未対応」を更新する
- `docs/sorahost-integration.md` — SoraHost ポータル（dev-sorahost）から管理 API を使う仕様（課金計算を含む）。
  `docs/examples/SuhmsAdminClient.php` は Laravel 用クライアント例。**dev-sorahost 側の実装はこのリポジトリでは行わない**
- `deploy/suhms.service`（systemd）、`deploy/nginx.conf`（TLS 終端。管理 API とノード間 API は公開しない）

## ライセンス

MIT ライセンス。依存クレートは `deny.toml` の許可リスト（MIT / Apache-2.0 / BSD / BSL-1.0 等の寛容なもの）に限る。
コピーレフト（GPL / AGPL / LGPL）の依存は追加しないこと。CI の cargo-deny がこれを検査する。

## 決定済みの設計方針

- **初期ハードウェア**: 3 ノード × 各 4 ディスク（計 12 ドライブ）。ノード間は 1〜10Gbps。
  ディスク・ノードの追加に対応すること（ハードコード禁止）。
- **冗長化はレプリケーション（Erasure Coding / パリティ / RAID 的な独自形式は使わない）**:
  オブジェクトはそのままの内容で普通のファイルとして、OS のファイルシステム（XFS / ext4）上に置く。
  コピーは**それぞれ別のノードのディスクに置く**（障害ドメイン = ノード）。単一ノード構成の場合は別々のディスクに置く。
  - **レプリカ数は 1 / 2 / 3（以上）から個別に選べる**。優先順位は
    オブジェクト（`x-amz-storage-class` ヘッダ）＞ バケット設定 ＞ クラスタのデフォルト（3）
    - storage class の対応表は設定ファイルで定義する（例: `STANDARD`=3、`STANDARD_IA`=2、`REDUCED_REDUNDANCY`=1）
    - レプリカ数がノード数を超える指定は拒否する
    - 書き込みの成功条件: 1→1 コピー、2→2 コピー、3→2 コピー（3 つ目はバックグラウンドで補う）
    - バケットのレプリカ数を後から変えたら、既存オブジェクトはバックグラウンドで複製を増やすか減らす
    - レプリカ 1 はディスク 1 本の故障でデータが消える。料金プランと利用規約で明示すること
    - 課金の保存量はレプリカ数を掛けずに論理サイズで数えるか、プラン単価で差をつける（課金設計で決める）
  - ファイル名はオブジェクトキーではなく内部 ID（`<disk>/objects/ab/cd/<id>`）にする。キーと場所の対応はメタデータ DB に持つ
  - 書き込みは temp → fsync → rename。レプリカの過半数（3 なら 2）が永続化した時点で成功を返し、残りはバックグラウンドで補う
  - 内容ハッシュ（BLAKE3 等）をメタデータに持ち、読み出し時と定期 scrub で照合する。壊れたコピーは健全なコピーから作り直す
  - ディスクやノードが死んだら、レプリカ数が足りないオブジェクトを他のディスクへ複製し直す（re-replication）
  - ディスクを追加したら、新規の書き込みは空きの多いディスクを優先する。既存データの移動は任意のバックグラウンド処理とする
- **メタデータ**: `trait MetadataStore` で抽象化し、SQLite / PostgreSQL / MariaDB を設定で選べるようにする（`sqlx` を想定）。
  SQLite は単一ノード構成と開発専用。マルチノードでは PostgreSQL（Patroni 等）か MariaDB（Galera 等）の HA 構成が前提。
  SQL は 3 方言すべてで動くものに限り、テストも 3 バックエンドで実行する。

## 未対応（既知の穴）

- `deny.toml` で RUSTSEC-2026-0459（同梱 SQLite の脆弱性）を一時除外中。sqlx が libsqlite3-sys >= 0.39 を許容したら更新して除外を外す
- PostgreSQL / MariaDB はこのホストでは未検証（CI の `metadata-backends` ジョブでサービスコンテナに対して store のテストを実行）。
  ローカルで試すなら `SUHMS_TEST_POSTGRES=postgres://... cargo test -p suhms-store`（テストごとに DB を作る）
- `[cluster.tls]` を設定しないとノード間通信は平文 HTTP（HMAC 認証のみ）。その場合はプライベートネットワークか WireGuard 等で保護すること
- オブジェクトタグ（Put/GetObjectTagging）が無いので、ライフサイクルのタグフィルタも未対応
- 既定チェックサム（CRC64NVME）は既定チェックサム導入前に保存したオブジェクト・パートには付かない（後から計算し直す処理はない）
- メタデータ更新前にクラッシュした場合や、オフラインのディスク上に残った孤立ファイルの GC がない
- ディスクの恒久撤去（decommission）時に replicas 行を消す管理操作がない
- `list_misreplicated` は objects の全件走査。数億件規模では見直しが必要
- repair は 1 ノードで実行する前提（複数ノードで同時実行しても壊れないが、余分なコピーが一時的にできうる）
- ノードの恒久撤去・リバランス（既存データの移動）は未対応

## 設計上の注意

- リクエスト/レスポンスのボディは常にストリーミングで扱い、オブジェクト全体をメモリに載せない
- アクセスキーのシークレットは SigV4 の検証に生の値が要るため、ハッシュ化ではなく暗号化して保存する
- テナント境界: データへのアクセスはすべてテナント ID でスコープし、テナント間アクセスのテストを必ず書く

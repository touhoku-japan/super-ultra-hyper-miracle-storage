# CLAUDE.md

S3 互換オブジェクトストレージの Rust 実装（旧 Laravel スケルトンからの置き換え）。
SoraHost の社内バックアップ＋顧客向け外販が目的。マルチテナントと、Erasure Coding による複数ディスク/複数ノードの冗長化が必須要件。

## コマンド

- テスト: `CARGO_BUILD_JOBS=1 cargo test --workspace`
- Lint: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
- 起動: `cargo run -p suhms-server -- -c config.toml serve`（`GET /_suhms/health` で疎通確認。認証不要）
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
    - `lib.rs` — `app()`: axum ルータ（ヘルスチェック + それ以外は s3s の S3 サービス）
    - `s3.rs` — `s3s::S3` 実装。リクエストごとに検証済みアクセスキー → テナント ID を引き、store にテナントを渡す。
      `KeyStore`（`S3Auth`）が SigV4 用にシークレットを復号して返す。body 読み取り中の s3s エラー（SHA-256 不一致等）は `body_error` で S3 エラーに戻す
    - `crypto.rs` — アクセスキーのシークレットを AES-256-GCM で暗号化（AAD = アクセスキー ID）。マスターキーは `SUHMS_MASTER_KEY` か `[auth] master_key_file`
    - `checksum.rs` — `x-amz-checksum-*`（ヘッダ、または aws-chunked のトレーラ）を body を流しながら検証する reader。
      不一致は EOF で読み取りエラーにし、store がコミット前に書き込みを捨てる
    - `admin.rs` — 管理サブコマンド
    - `tests/` — `common/mod.rs` の自前 SigV4 署名器で、実ルータを in-process で叩く E2E テスト
  - `store/` — ストレージエンジン
    - `disk.rs` — ディスク = 既存ディレクトリ。`.suhms/format.json` の UUID で識別（マウント順が変わっても同一視）。
      ルートが無ければ作らない（未マウント時にルート FS へ書くのを防ぐ）。中身のある未フォーマットのディレクトリは使わない
    - `placement.rs` — 配置（別ノード優先 → 同ノード別ディスク、空き容量の多い順）と ACK 閾値（過半数）
    - `blob.rs` — 全レプリカへの同時ストリーミング書き込み（MD5=ETag と BLAKE3 を同時計算）、検証付きコピー
    - `meta/` — `MetadataStore` trait と SQLite 実装。スキーマは `meta/migrations/<backend>/NNNN_*.sql`（リリース済みのマイグレーションは編集しない）
    - `list.rs` — S3 の ListObjects 意味論（prefix / delimiter / marker / continuation）。キー順は UTF-8 バイト順
    - `store.rs` — `Store`: テナント・バケット・オブジェクト操作（すべて owner = テナント ID でスコープ。他テナントのバケットは AccessDenied）、
      コピー、マルチパート、`scrub()`、`repair()`
    - マルチパート: アップロード開始時にディスクを決め、パートは各ディスクの `.suhms/multipart/<upload>/<blob>` に置く。
      完了時は各ディスク上でローカルにパートを連結して普通のオブジェクトファイルにする（パートごとに BLAKE3 を検証）。
      ETag は S3 と同じ `md5(パートMD5の連結)-N`。完了処理中は s3s がキープアライブの空白を送る
- S3 プロトコル層は `s3s` クレート（Apache-2.0。`openssl` feature は使わない）
- 対応済み S3 操作: ListBuckets, Create/Head/Delete Bucket, GetBucketLocation, GetBucketVersioning（常に未設定）,
  Put/Get（Range）/Head/Delete Object, DeleteObjects, ListObjects V1/V2（encoding-type=url 対応）, presigned URL,
  CopyObject（COPY/REPLACE）, Create/UploadPart/UploadPartCopy/Complete/Abort MultipartUpload, ListParts, ListMultipartUploads,
  条件付きリクエスト（GET/HEAD の If-Match / If-None-Match / If-(Un)Modified-Since、PUT の If-None-Match: *）

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
- PostgreSQL / MariaDB バックエンド未実装（`meta::connect` がエラーを返す）
- チェックサムは検証するが保存しないので、GET で `x-amz-checksum-*` を返さない。マルチパート全体のチェックサム（COMPOSITE / FULL_OBJECT）も未対応
- PUT の If-Match、ListMultipartUploads の delimiter は未対応
- 放置されたマルチパートアップロードの自動削除なし（バケット削除時のみ中止される）
- scrub / repair は手動実行のみ（定期実行なし）
- メタデータ更新前にクラッシュした場合や、オフラインのディスク上に残った孤立ファイルの GC がない
- ディスクの恒久撤去（decommission）時に replicas 行を消す管理操作がない
- `list_misreplicated` は objects の全件走査。数億件規模では見直しが必要
- マルチノード（ノード間転送）は Phase 4

## 設計上の注意

- リクエスト/レスポンスのボディは常にストリーミングで扱い、オブジェクト全体をメモリに載せない
- アクセスキーのシークレットは SigV4 の検証に生の値が要るため、ハッシュ化ではなく暗号化して保存する
- テナント境界: データへのアクセスはすべてテナント ID でスコープし、テナント間アクセスのテストを必ず書く

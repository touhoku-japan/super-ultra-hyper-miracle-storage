# CLAUDE.md

S3 互換オブジェクトストレージの Rust 実装（旧 Laravel スケルトンからの置き換え）。
SoraHost の社内バックアップ＋顧客向け外販が目的。マルチテナントと、Erasure Coding による複数ディスク/複数ノードの冗長化が必須要件。

## コマンド

- テスト: `CARGO_BUILD_JOBS=1 cargo test --workspace`
- Lint: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings`
- 起動: `cargo run -p suhms-server -- --config config.example.toml`（`GET /health` で疎通確認）

開発ホストは 2 コア / 2.9GB RAM で他アプリと共有しているため、ビルドは `CARGO_BUILD_JOBS=1` で行う。
release ビルドは CI 側で行う想定。

## 構成

- Cargo workspace。クレートは `crates/*`
  - `server/` — バイナリ `suhms`。設定（TOML、`deny_unknown_fields`）、tracing、graceful shutdown
- 予定: `store/`（EC ストレージエンジン、trait で抽象化）、`auth/`（テナント・アクセスキー・SigV4）、`admin-cli/`
- S3 プロトコル層は `s3s` クレートを採用（決定済み、Apache-2.0。`openssl` feature は使わない）

## ライセンス

MIT ライセンス。依存クレートは `deny.toml` の許可リスト（MIT / Apache-2.0 / BSD 等の寛容なもの）に限る。
コピーレフト（GPL / AGPL / LGPL）の依存は追加しないこと。CI の cargo-deny がこれを検査する。

## 決定済みの設計方針

- **初期ハードウェア**: 3 ノード × 各 4 ディスク（計 12 ドライブ）。ノード間は 1〜10Gbps。
  ディスク・ノードの追加に対応すること（ハードコード禁止）。
- **Erasure Coding**: 初期構成のデフォルトは 12 ドライブで 1 セットの 8+4（容量効率 67%）。
  各ノードに 4 シャードずつ配置するので、1 ノード全損またはディスク 4 本の故障まで読める。
  データ:パリティ比とセット幅は設定で変えられるようにし、シャードはノード間に均等に配置する。
  増設は「プール」単位で行い、既存データのリバランスはしない。
- **メタデータ**: `trait MetadataStore` で抽象化し、SQLite / PostgreSQL / MariaDB を設定で選べるようにする（`sqlx` を想定）。
  SQLite は単一ノード構成と開発専用。マルチノードでは PostgreSQL（Patroni 等）か MariaDB（Galera 等）の HA 構成が前提。
  SQL は 3 方言すべてで動くものに限り、テストも 3 バックエンドで実行する。

## 設計上の注意

- リクエスト/レスポンスのボディは常にストリーミングで扱い、オブジェクト全体をメモリに載せない
- アクセスキーのシークレットは SigV4 の検証に生の値が要るため、ハッシュ化ではなく暗号化して保存する
- テナント境界: データへのアクセスはすべてテナント ID でスコープし、テナント間アクセスのテストを必ず書く

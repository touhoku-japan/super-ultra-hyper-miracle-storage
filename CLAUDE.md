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
- S3 プロトコル層は `s3s` クレートを採用（決定済み、Apache-2.0。`openssl` feature は使わない）。メタデータは PostgreSQL HA が推奨案（未確定）

## ライセンス

BSD ライセンスで配布予定。依存クレートは `deny.toml` の許可リスト（MIT / Apache-2.0 / BSD 等の寛容なもの）に限る。
コピーレフト（GPL / AGPL / LGPL）の依存は追加しないこと。CI の cargo-deny がこれを検査する。

## 設計上の注意

- リクエスト/レスポンスのボディは常にストリーミングで扱い、オブジェクト全体をメモリに載せない
- アクセスキーのシークレットは SigV4 の検証に生の値が要るため、ハッシュ化ではなく暗号化して保存する
- テナント境界: データへのアクセスはすべてテナント ID でスコープし、テナント間アクセスのテストを必ず書く

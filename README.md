# super-ultra-hyper-miracle-storage
Super-Ultra-Hyper-Miracle Storage is HTTP Storage Server

S3 互換のオブジェクトストレージ（Rust 製）。SoraHost の社内バックアップおよび顧客向けの外販を想定し、
マルチテナント・Erasure Coding による複数ディスク/複数ノードの冗長化を目標とする。

## 機能

- S3 互換 API（SigV4 / presigned URL / aws-chunked、マルチパート、コピー、Range、条件付きリクエスト、チェックサム検証）
- オブジェクトは**普通のファイルとしてそのまま保存**。レプリカ数 1〜3 をバケット/ストレージクラスごとに選択、複数ノードに分散
- 複数ノードのクラスタ（ノード障害時も読み書き継続、復旧後に自動修復）、scrub によるビット腐敗検出と修復
- メタデータ DB: SQLite（単一ノード）/ PostgreSQL / MariaDB
- マルチテナント: テナントごとのアクセスキー、容量クォータ、停止、レート制限、課金用の使用量計測、管理 API
- バージョニング、Object Lock（WORM）、ライフサイクル、CORS、公開バケット

ドキュメント: [`docs/sorahost-integration.md`](docs/sorahost-integration.md)（SoraHost 連携・課金）、
[`deploy/`](deploy/)（systemd・nginx）、[`config.example.toml`](config.example.toml)（全設定項目）

## 使い方

```sh
cargo build --release -p suhms-server
cp config.example.toml config.toml   # disks / metadata_url / master_key_file を環境に合わせる
./target/release/suhms gen-master-key > /etc/suhms/master.key && chmod 600 /etc/suhms/master.key
./target/release/suhms -c config.toml tenant create my-customer
./target/release/suhms -c config.toml key create my-customer   # access key / secret を表示（一度だけ）
./target/release/suhms -c config.toml serve
```

クライアント例（rclone）:

```sh
export RCLONE_CONFIG_S_TYPE=s3 RCLONE_CONFIG_S_PROVIDER=Other \
  RCLONE_CONFIG_S_ENDPOINT=http://127.0.0.1:9000 RCLONE_CONFIG_S_FORCE_PATH_STYLE=true \
  RCLONE_CONFIG_S_ACCESS_KEY_ID=... RCLONE_CONFIG_S_SECRET_ACCESS_KEY=...
rclone mkdir s:backups && rclone copy ./data s:backups/
```

## 開発

```sh
CARGO_BUILD_JOBS=1 cargo test --workspace
curl http://127.0.0.1:9000/_suhms/health
```

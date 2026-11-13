# super-ultra-hyper-miracle-storage
Super-Ultra-Hyper-Miracle Storage is HTTP Storage Server

S3 互換のオブジェクトストレージ（Rust 製）。SoraHost の社内バックアップおよび顧客向けの外販を想定し、
マルチテナント・Erasure Coding による複数ディスク/複数ノードの冗長化を目標とする。

## 開発

```sh
cargo test
cargo run -p suhms-server -- --config config.example.toml
curl http://127.0.0.1:9000/health
```

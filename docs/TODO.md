# やり残し（バックログ）

2026-11-13 時点、Rust 版（Phase 0〜7）を main にマージした後に残っている作業。
片付けたら項目を消すか「済」に移し、`CLAUDE.md` の「未対応」も合わせて更新すること。

## 本番投入前に必要

1. **実機検証** — 3 台実機でのクラスタ、PostgreSQL（Patroni 等）/ MariaDB（Galera 等）の HA 構成、負荷・性能試験。
   これまでは 1 プロセス内の 3 ノードと CI のサービスコンテナでしか確認していない
2. **PostgreSQL / MariaDB をこのホストで未検証** — CI では 3 バックエンドとも通っているが、実 DB サーバでは未確認
3. **ノード間通信の暗号化** — 現状は平文 HTTP + HMAC 認証のみ。プライベートネットワーク / WireGuard で保護するか、TLS 対応を入れる
4. **dev-sorahost（ポータル）側の実装** — 申込、アクセスキー発行、使用量取得、Stripe 課金連携。
   仕様は `docs/sorahost-integration.md`、PHP クライアント例は `docs/examples/SuhmsAdminClient.php`
5. **RUSTSEC-2026-0459 の一時除外を外す** — sqlx が libsqlite3-sys >= 0.39 を許容したら更新して `deny.toml` から除外を削除

## 運用面

6. **孤立ファイルの GC** — メタデータ更新前のクラッシュや、オフラインだったディスクに残ったファイルを回収する処理がない
7. **ディスクの恒久撤去（decommission）** — 撤去したディスクの replicas 行を消す管理操作がない
8. **ノードの恒久撤去・リバランス** — 既存データを他ノードへ移す処理がない
9. **repair の多重実行** — 1 ノード（leader）実行が前提。複数ノードで同時に走っても壊れないが、余分なコピーが一時的にできうる
10. **`list_misreplicated` の全件走査** — 数億オブジェクト規模ではインデックスや差分方式への見直しが必要

## S3 互換性

11. **チェックサムの保存と返却** — `x-amz-checksum-*` を検証するが保存しないので GET/HEAD で返せない。
    マルチパート全体のチェックサム（COMPOSITE / FULL_OBJECT）も未対応
12. **PUT の If-Match** — 条件付き書き込み（If-None-Match: * のみ対応済み）
13. **ListMultipartUploads / ListObjectVersions の delimiter**（CommonPrefixes）
14. **versionId 指定のコピー元** — CopyObject / UploadPartCopy の `x-amz-copy-source: bucket/key?versionId=...`
15. **ライフサイクルの残り** — 削除マーカーだけが残ったキーの自動削除（ExpiredObjectDeleteMarker）、
    ストレージクラス移行（transition）
16. **AWS CLI での互換確認** — このホストに未インストールのため rclone でのみ確認済み

## 実装しないと決めたもの

- **サーバ側暗号化（SSE）** — 「ファイルをそのまま置く」方針と矛盾するため。保存時暗号化はディスク側（LUKS 等）で行う

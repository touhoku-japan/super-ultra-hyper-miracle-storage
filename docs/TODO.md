# やり残し（バックログ）

2026-11-13 時点、Rust 版（Phase 0〜7）を main にマージした後に残っている作業。
片付けたら項目を消すか「済」に移し、`CLAUDE.md` の「未対応」も合わせて更新すること。

## 本番投入前に必要

1. **実機検証** — 3 台実機でのクラスタ、PostgreSQL（Patroni 等）/ MariaDB（Galera 等）の HA 構成、負荷・性能試験。
   これまでは 1 プロセス内の 3 ノードと CI のサービスコンテナでしか確認していない
2. **PostgreSQL / MariaDB をこのホストで未検証** — CI では 3 バックエンドとも通っているが、実 DB サーバでは未確認
4. **dev-sorahost（ポータル）側の実装** — 申込、アクセスキー発行、使用量取得、Stripe 課金連携。
   仕様は `docs/sorahost-integration.md`、PHP クライアント例は `docs/examples/SuhmsAdminClient.php`
5. **RUSTSEC-2026-0459 の一時除外を外す** — sqlx が libsqlite3-sys >= 0.39 を許容したら更新して `deny.toml` から除外を削除
   （2026-11-13 確認: 最新の sqlx 0.9.0 / sqlx-sqlite 0.9.0 は `libsqlite3-sys >=0.30.1, <0.38.0` のままで未対応）

## 済

- ~~11. チェックサムの保存と返却（COMPOSITE / FULL_OBJECT 含む）~~
- ~~12. PUT の If-Match~~（CompleteMultipartUpload の If-Match / If-None-Match も）
- ~~13. ListMultipartUploads / ListObjectVersions の delimiter~~
- ~~14. versionId 指定のコピー元~~
- ~~15. ExpiredObjectDeleteMarker、transition（ストレージクラス＝レプリカ数の変更）~~
- ~~17. チェックサムの残り~~（未指定時の CRC64NVME、MD5 / SHA512 / xxHash 系）
- ~~18. ライフサイクルの残り~~（Date、NewerNoncurrentVersions、サイズ・タグフィルタ）
- ~~19. オブジェクトタグ~~（Put/Get/DeleteObjectTagging、`x-amz-tagging`、タグ数ヘッダ）
- ~~6. 孤立ファイルの GC~~（`suhms gc`、各ノードで日次。ゴミ箱経由で削除）
- ~~7. ディスクの恒久撤去~~（`suhms disk drain|remove`）
- ~~8. ノードの恒久撤去・リバランス~~（`suhms node drain|remove`、`suhms rebalance`）
- ~~9. repair の多重実行~~（DB のリースで 1 台ずつ。全ノードでスケジュール）
- ~~10. `list_misreplicated` の全件走査~~（`repair_queue` によるキュー駆動。全件確認は週 1 の `repair_full`）
- ~~16. AWS CLI での互換確認~~（2026-11-14、AWS CLI 2.40.3 で 66 項目すべて成功。`scripts/awscli-compat.sh`）
- ~~3. ノード間通信の暗号化~~（`[cluster.tls]` の相互 TLS。`docs/cluster-tls.md`）

## 実装しないと決めたもの

- **サーバ側暗号化（SSE）** — 「ファイルをそのまま置く」方針と矛盾するため。保存時暗号化はディスク側（LUKS 等）で行う

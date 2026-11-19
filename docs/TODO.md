# やり残し（バックログ）

2026-11-13 時点、Rust 版（Phase 0〜7）を main にマージした後に残っている作業。
片付けたら項目を消すか「済」に移し、`CLAUDE.md` の「未対応」も合わせて更新すること。

## 本番投入前に必要

1. **実機検証** — 3 台実機でのクラスタ、PostgreSQL（Patroni 等）/ MariaDB（Galera 等）の HA 構成、負荷・性能試験。
   これまでは 1 プロセス内の 3 ノード、CI のサービスコンテナ、開発ホスト 1 台での 3 プロセス構成
   （2026-11-19、PG・mTLS。障害注入・release ビルドでの負荷・rclone も確認）でしか確認していない
4. **dev-sorahost（ポータル）側の実装** — 申込、アクセスキー発行、使用量取得、Stripe 課金連携。
   仕様は `docs/sorahost-integration.md`、PHP クライアント例は `docs/examples/SuhmsAdminClient.php`

## 任意（あると良い）

- **ポータルからのワンタイムログイン** — SoraHost ポータルにログイン済みの顧客を、そのまま Web コンソールへ移す。
  管理 API で短命のトークンを発行し、コンソールの `api/login` で交換する形を想定（`docs/webui.md`）

## 済

- ~~RUSTSEC-2026-0459 の一時除外~~（2026-11-19。sqlx 0.9.1 で libsqlite3-sys 0.40 に更新し、`deny.toml` から除外を削除）
- ~~PostgreSQL / MariaDB を開発ホストで検証~~（2026-11-19。store のテストと awscli-compat。手順は `CLAUDE.md` の「開発ホスト」）
- ~~開発ホストでの検証で見つかった点~~（2026-11-19）: 制御文字を含むキーで一覧が 500 になる（書き込み時に拒否）、
  落ちた直後のノードを書き込み先に選ぶと 2/2 の PUT が 503（開く段階で失敗を検出して別ディスクに差し替え）、
  範囲指定 GET が壊れたコピーに気づかない（読んだコピーを後で全体照合）、書き込み時のコピー削減、s3s のクライアントエラーの ERROR ログ
- ~~固まったノードへの要求が永久に待つ~~（2026-11-19。待っている間に ping して生存を確かめる `HttpPeer::alive`）、
  repair が同時に削除されたオブジェクトで ERROR を出す、5xx の ERROR ログの重複
- ~~1 ノード内の書き込みが 1 本のディスクに集中する~~（2026-11-19。空き容量が 5% 以内のディスクはランダムに選ぶ。`placement::choose`）
- ~~server クレートの E2E を PostgreSQL / MariaDB でも実行~~（2026-11-19。`tests/common` の `metadata_url`、CI の `metadata-backends` ジョブ）
- ~~クラスタの耐障害性の追加修正~~（2026-11-19）: 2 コピーのマルチパートが途中のノード障害で完了できない（パートの差し替えと、
  完了時に欠けたパートをコピー）、全体読みで見つけた他ノードの壊れたコピーを捨てる、書き込みに失敗したノードをすぐ ping して外す、
  マルチパート完了時のパート照合を CRC に（BLAKE3 の 2 回目を省く）
- ~~Web コンソールのジョブ状態の共有~~（2026-11-15。メタデータ DB の `job_runs` に全ノードの最後の実行を記録。定期実行も含む。`docs/webui.md`）
- ~~Web コンソールのブラウザ E2E テスト~~（2026-11-15。`web/e2e/` の Playwright、CI の `e2e` ジョブ。開発ホストでも `cups-libs` を入れて実行可能）
- ~~Web コンソール~~（2026-11-15。顧客用 `[console]` と運用者用 `/ui/`。`docs/webui.md`）
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
- ~~既存オブジェクトへのチェックサム後付け~~（`suhms checksum-backfill`、leader で自動実行）
- ~~直せないオブジェクトの repair 再試行間隔~~（10 分〜6 時間のバックオフ。ディスクの追加・復帰で即再試行）
- ~~16. AWS CLI での互換確認~~（2026-11-14、AWS CLI 2.40.3 で 66 項目すべて成功。`scripts/awscli-compat.sh`）
- ~~3. ノード間通信の暗号化~~（`[cluster.tls]` の相互 TLS。`docs/cluster-tls.md`）

## 実装しないと決めたもの

- **サーバ側暗号化（SSE）** — 「ファイルをそのまま置く」方針と矛盾するため。保存時暗号化はディスク側（LUKS 等）で行う

# セキュリティポリシー / Security Policy

## 脆弱性の報告

脆弱性を見つけた場合は、**公開の Issue・Pull Request・Discussions には書かず**、
GitHub の非公開報告（Private vulnerability reporting）で報告してください。

1. このリポジトリの **Security** タブを開く
2. **Report a vulnerability** を押す
3. 内容（影響範囲、再現手順、可能なら PoC・該当バージョン/コミット）を記入して送信

報告内容はメンテナーだけが見られます。

### 特に重視する問題

- テナント境界の突破（他テナントのバケット・オブジェクト・使用量へのアクセス）
- SigV4 / presigned URL / 管理 API・ノード間 API の認証回避
- アクセスキーのシークレットやマスターキーの漏えい
- 保存データの破損・消失につながる問題（レプリケーション、repair、gc、Object Lock の回避など）
- サービス停止を引き起こす入力（メモリ・ディスクの枯渇など）

## 対応の流れ

- 受領の連絡: 原則 5 営業日以内
- 内容を確認し、修正方針と公開予定を報告者と相談します
- 修正版のリリースに合わせて GitHub Security Advisory を公開し、必要に応じて CVE を申請します
- 希望があれば、アドバイザリに報告者のクレジットを記載します

## 対象バージョン

`main` ブランチの最新版のみを修正対象とします。

---

## Reporting a vulnerability (English)

Please **do not** open public issues for security problems. Use GitHub's
[private vulnerability reporting](../../security/advisories/new) instead
(Security tab → "Report a vulnerability"). We aim to acknowledge reports within
5 business days and will coordinate disclosure through a GitHub Security Advisory.
Only the latest `main` is supported.

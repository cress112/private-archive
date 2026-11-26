# 実AWSでのアプリケーション受入

状態: **完了（全43件完了）**。台帳の段階: `complete`。

利用者の指示により、AWS構成の参照・plan/apply・権限追加は実施しない。対象は既存の `cress-archived-data` バケット内に作った専用prefix `acceptance-c07215841ef54eddb1804efb41a9af73/` だけ。IAMユーザ・ロール・バケットは削除しない。

| 確認 | 結果 |
| --- | --- |
| 3方向コピーの内容一致 | 成功 |
| 属性・一覧・再照合 | 成功 |
| 通常DBのS3往復 | 成功 |
| 条件付きGET 304／PUT 412・現行本文の保持 | 成功 |
| アプリ経由の削除 | 成功 |
| 実S3へのGUI再照合・CLIとのDB共有 | 成功 |
| Deep Archive復元後の取得・内容一致 | 成功 |
| 試験後の空バケット | 成功 |

[数量台帳と実結果](validation/aws-live.json)を正本とする。CLI/GUI本体は既存の配備バイナリを使用し、Rustの再ビルドは行っていない。試験データは全て清掃し、バケットが空であることを確認した。

## 費用管理

- 許可上限: 追加1 USD。試験計画の予備枠: 0.20 USD。無料枠を前提にしない。
- 独立した上限: S3要求の予約枠400、転送の予約枠20MiB、作成キー4個、本文64KiB、インデックス最大4MiB、復元要求1回（Bulk・1日）。
- 現在の要求予約枠: 244。転送予約枠: 16908288 bytes。
- 現在の保守的な予約額: 0.05130829 USD。要求を一律0.0001 USD、転送を1 USD/GB、保存・最低180日・追加メタデータ・復元に0.01 USDを確保して計算する。全上限を使っても0.07097152 USD。
- これは実請求額ではない。請求情報の遅延を停止条件にせず、操作前に数量枠を予約し、通常操作を上限368要求で停止して清掃枠32要求を残す。
- 失敗・応答不明の操作も枠を消費する。SDK/AWS CLI/curlの要求再試行は無効。復元要求を繰り返さない。

[単価の根拠](validation/aws-live-pricing.json): [S3 us-east-1公開料金](https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonS3/current/us-east-1/index.json)、[Deep Archive us-east-1公開料金](https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonS3GlacierDeepArchive/current/us-east-1/index.json)。

## 復元待ちと後始末

復元要求時刻: `2026-11-23T10:13:13.615464+00:00`。待機期限: `2026-11-26T10:13:13.615482+00:00`。最新の復元状態: `{"StorageClass": "DEEP_ARCHIVE", "ContentLength": 65536, "Restore": "ongoing-request=\"false\", expiry-date=\"Thu, 26 Nov 2026 00:00:00 GMT\""}`。

試験専用の `tests/aws/watch.py` が1時間ごとに状態を確認する。アプリには自動監視・更新機能を追加しない。完了後はアプリで取得しSHA256を比較、追跡済みキーだけを削除し、バケットのデータ一覧が空であることを確認する。期限・試験枠・エラーで継続できない場合は未完了として記録し、追跡済み試験データを清掃する。

監視ログ: `target/aws-live-watch.log`。監視はこのMacが動作し、AWS認証・通信が利用できる間に進む。手動で再開する場合は、監視の状態を台帳で確認してから次を使う（ローカルロックで重複実行を防ぐ）。

```sh
/bin/bash scripts/private-archive-session.sh python3 tests/aws/acceptance.py check
```

## 検証スクリプトの修正記録

初回の304判定でAWS CLIの終了コードを255と誤認したが、実際は254で、S3の304応答自体は成功していた。また、再開用メソッド名とPath属性の衝突を修正した。どちらも台帳へ記録し、停止時に専用キーを清掃した。成功済みの3方向コピーやDB往復を再試験せず、予約枠を保持したまま未完了項目だけを継続した。インフラやアプリの不具合として扱っていない。

AWS CLI 2.18.3はPutObjectのIf-Match引数に未対応のため、この1要求だけcurl 8.7.1のSigV4を使い、資格情報は標準入力から渡した。現行ETagが変化したことを確認してから古いETagを送り、412と現行ETag不変を確認した。製品自身の412通知・再送しない経路は従来のHTTP/GUI試験を根拠とする。

画像: [実S3から得た記録の詳細](validation/aws-live-gui-file.png)、[削除済み範囲へのGUI再照合](validation/aws-live-gui-empty.png)。

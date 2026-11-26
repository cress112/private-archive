# 実AWSの最終アプリ受入

実施中の結果は [実AWS検証記録](../../docs/AWS_VALIDATION.md)、数量・予約額・作成キーは [台帳](../../docs/validation/aws-live.json)。本番用の汎用テストではなく、このプロジェクトの最終受入に限定したドライバ。

`acceptance.py`は既存の認証解決スクリプトから起動し、渡された環境認証で製品CLI/GUIと限定したS3データAPIだけを使う。AWS構成の参照・plan/apply・権限追加は行わない。3方向コピー・通常DB往復・条件付き要求・GUIの後、Deep Archive64KiBをBulk/1日で1回だけ要求する。

```sh
# 現在の試験台帳を使う状態確認。新しい試験を開始しない。
/bin/bash scripts/private-archive-session.sh python3 tests/aws/acceptance.py check
# 明示的な中止・清掃（この台帳で追跡したキーだけ）
/bin/bash scripts/private-archive-session.sh python3 tests/aws/acceptance.py cleanup
# ネットワークなしで、保存済み台帳から文書の状態だけを更新
python3 tests/aws/watch.py --publish-only
```

`start`は既存台帳がある場合に拒否する。失敗を隠すために台帳を消して試験をやり直さない。`restore`も要求済みフラグを送信前に保存し、同じ復元要求を繰り返さない。`finish-standard`は今回の検証スクリプトの判定修正後に、成功済みのコピー試験を繰り返さず残件だけを完了するために使った経路。

操作前に要求・転送枠を予約し、S3要求400枠・転送20MiB（HTTPとアップロードの予備4MiBを含む）を超える操作は起動しない。通常操作は368枠で停止し、清掃用に32枠を残す。キーは一意prefix内の4個に固定し、最初のバケットが空でなければ停止する。失敗/応答不明も枠を消費し、要求を自動再試行しない。SDKは既存のretry無効設定、AWS CLIは`AWS_MAX_ATTEMPTS=1`を使う。

`watch.py`は試験専用の状態確認と後始末を行う。1時間間隔・最大74回・要求から72時間の期限・台帳の要求/転送枠を守り、復元完了後の取得/内容一致と清掃後にだけ完了と記録する。エラーが起きたら清掃を1回試みて停止する。`target/aws-live-monitor.json`に監視状態、`target/aws-live-watch.log`に経過を保存する。ローカルロックでドライバの重複実行を防ぎ、既存監視が動いているときに二重起動しない。

CLIとGUIのバイナリ・署名済み.appは変更しない。試験用GUIは既存fixtureを使い、接続先上書きは指定せず、実S3から得た通常DBを描画して選択範囲の再照合を実行する。資格情報を引数・台帳・文書・一時ファイルへ書かない。curlのSigV4用資格情報は標準入力だけで渡す。

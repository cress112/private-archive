# 完成範囲の受入と運用引継ぎ

2026-11-23、利用者の指示によりP3-11の実AWSアプリ動作受入を開始した。追加費用上限1 USD。AWS構成の参照・plan/apply・権限追加は行わず、不具合があればプロジェクト内IaCの確認・修正までとする。

<!-- aws-acceptance:start -->
実AWS受入: **完了（全43件完了）**。[検証結果と費用台帳](AWS_VALIDATION.md)。
<!-- aws-acceptance:end -->

## 要件と証拠

| 要件 | 実装・検証した範囲 | 証拠と残る実AWS確認 |
| --- | --- | --- |
| RQ-01 到達点 | CLIを先行し、共用コア/設定/インデックスからSlint GUIを提供。GUIの操作範囲は閲覧・再読込・再照合・設定編集 | [Phase 1](PHASE1_VALIDATION.md)、[Phase 2](PHASE2_VALIDATION.md)、[Phase 3](PHASE3_VALIDATION.md)、[TASKS](archived/TASKS.md) |
| RQ-02 コピー・対象指定 | 3方向cp、再帰/深さ、完全一致とprefix、上書き、隠し名の既定対象化、明示ignore | Phase 1/2のDocker・障害/CLI試験。実AWSの3方向転送とSHA256一致も成功 |
| RQ-03 明示的な再照合 | 範囲限定、正常空成功、全件ignore失敗、部分取得の保持、GUIは既知bucketだけ | Phase 2/3のHTTP障害試験・Docker CLI/GUI共用。実S3に対するCLI/GUI照合と正常空範囲の反映も成功 |
| RQ-04 更新対象の選択 | 実行内の実ストア観測→付属index反映→実操作、全対象の事前解決 | Phase 1/2のDiskPlan・外部追加・古いindex・index異常・要求0件試験 |
| RQ-05 保存・復元 | standard/archived、期間d/w、Bulk/Standard、全URIの事前検査・重複排除、受付後終了 | [復元を含むPhase 2](PHASE2_VALIDATION.md)。実Deep ArchiveのBulk/1日要求を受付済み。復元後の内容一致と清掃は本書の状態欄を参照 |
| RQ-06 インデックス | 保存場所非依存の同一形式、通常参照API0、条件付き取得/保存、競合時の自動再送なし、使用中index保護 | Phase 2の同時利用/移行/競合試験、Phase 3のS3 index 412と次回閲覧時の再送なし。実S3の通常DB往復、条件付き304/412と現行本文の保持も成功 |
| RQ-07 失敗・中断 | 初回失敗で停止、途中成功保持、成否不明、SDK再試行なし、multipart後始末、共通50秒停止予算 | Phase 1/2の故障注入・縮小実転送、Phase 3の通信中選択/取消/閉じる。Lifecycle等のAWS構成参照は利用者指示により対象外 |
| RQ-08 同時実行 | 短いDB transaction、設定の原子的保存、取込中の別更新保持、GUI/CLI共用 | Phase 2の複数プロセス試験とPhase 3の明示再読込/再照合。実データ全体の原子性は要求しない |
| RQ-09 認証・インフラ | 外部profileをCLI/GUI wrapperとnative launcherで解決し、本体へ認証環境変数を渡す。SDK標準認証チェーン、最小権限のTerraform、access keyをIaCへ保存しない | [IaC](../terraform/environments/main/README.md)、[CLI導入](CLI_SETUP.md)、[GUI導入](../crates/gui/README.md)。実AssumeRoleと必要なS3操作は成功。AWS構成の参照・適用は利用者指示により対象外 |
| RQ-10 ファイル | symlink/特殊ファイルの拒否、ignore、空dir非保存、元属性best effort、内容検証 | Phase 1/2の属性/特殊ファイル・3方向内容照合。実S3の3方向コピーで元の更新日時・UNIX modeを含む属性と内容一致を確認 |
| RQ-11 設定・CLI | 共有TOML、cfgとGUI編集、0/1/2/130、lsのみ10項目JSON、ログ抑制と結果の独立、6引数の一時上書き | Phase 2のCLI契約とPhase 3の設定保存/取消/既定復帰、通常起動・不正引数検証 |
| RQ-12 非機能・受入 | CLIの1万/10万/100万件、GUIの有界ページ/32MiB行cache、Mac実GUI、同形式DBのARM CLI移行 | [移植性記録](PORTABILITY_VALIDATION.md)。P3-10でGUI100万件3ケース/RSS最大77.91MiB・行cache32MiB以下、Linux ARM64 GUIの実描画/基本操作も確認。ARM Macは互換性調査まで。100GB実本文転送は合意により対象外 |

## 利用する成果物

- 通常CLI: `target/bin/private-archive`。[導入・設定・運用・復旧](CLI_SETUP.md)。
- 通常GUI: `target/bin/private-archive-gui`、macOS `target/apps/Private Archive.app`。[GUI操作とDocker接続](../crates/gui/README.md)。
- 認証ラッパー: [CLI](../scripts/private-archive.sh)、[GUI](../scripts/private-archive-gui.sh)。起動・認証処理の正本はプロジェクト内の `scripts/`。
- IaC: [terraform/environments/main](../terraform/environments/main/README.md)。運用で使い続ける構成であり、試験後destroyしない。
- 通常版は検証用endpoint/driver引数を拒否する。fixtureバイナリは検証専用とし、実運用のwrapper/.appへ混ぜない。

## 実AWS受入の結果と残件

[実AWSの検証記録](AWS_VALIDATION.md)と[数量台帳](validation/aws-live.json)を参照。3方向コピー、属性、一覧・再照合、通常DBのS3往復、条件付きGET 304／PUT 412、アプリ経由の削除、実S3に対するGUI再照合は成功した。Deep Archiveの復元要求を1回送信した。復元後取得・内容一致・最後の清掃の最新結果は、本書の状態欄と実AWS検証記録を参照する。受付だけでP3-11を完了扱いにしない。

追加費用上限1 USDに対し、試験専用の予備枠0.20 USDを置く。要求予約400回・転送予約20MiB・作成キー4個・Deep Archive64KiB1個・Bulk/1日の要求1回に絞り、操作前に台帳で枠を消費する。失敗や応答不明も枠へ含める。保存・最低180日分・追加メタデータも保守的な予約額へ含める。実請求額は確定値として扱わない。

待機は最大72時間。試験外データを削除せず、追跡済み試験キーだけを清掃する。空バケット・IAMロール/ユーザ・運用IaC/stateを残す。今回の64KiB転送は単一PUTの範囲で、multipartは開始しない。利用者指示によりAWS構成・他のmultipart状態の参照や適用は行わない。

初期タスク表は`archived/TASKS.md`に保存した凍結時点の記録であり、今後は更新しない。P3-11の現在地は本書の状態欄と実AWS検証記録に反映する。深い復元後の内容一致と清掃を含む全受入チェックが成功したときだけ、全43件完了とする。

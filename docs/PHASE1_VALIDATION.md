# Phase 1 検証記録

2026-11-16。P1-09〜P1-15の成果と受入範囲を記録する。前段は[コピーコア・SQLite検証](CORE_COPY_VALIDATION.md)、契約は[転送計画](archived/TRANSFER_VALIDATION.md)、操作は[CLI導入手順](CLI_SETUP.md)。

## 属性・後始末・CLI

- `private-archive-version=1`と`private-archive-mtime` / `private-archive-created`（UTC RFC3339、nanoseconds）/ `private-archive-mode`（8進数）をS3 user metadataへ保存する。ownerは保存しない。HEAD/GETで読み戻し、single copyは元metadataのCOPY、multipart copyは取得した元属性をCreateへ渡す。
- ローカルでは検証済み一時ファイルへmtime/creation/modeをbest effortで設定し、その後に置換する。属性が不明・不正・OS非対応・設定失敗でも本文成功ならwarning付き成功。macOSで秒未満のmtime/creationとmodeの復元、1970年以前のschema往復、不正mode時の本文成功を確認した。Linuxでcreation復元を成功扱いにしない。
- CancelContextは最初の取消/要求操作失敗時刻を保持する。最大30秒の要求収束、最大15秒の当該ID Abort、その後の付属処理が最初のstop+50秒を越えて予算を取り直さない。index/表示は最大stop+47秒まで、manifestは残り3秒。時間切れのmanifestは場所を診断して保持し、自動再開しない。
- upload/copyの最大4パート制御を共通PartSchedulerへ移し、完了判定も共有する。uploadの最大本文所有は4送信+1待機+1組立て=96MiB以内。別の全本文collect・自動再送は行わない。
- CLIは実コアを呼び、stdoutに各成功の元/宛先/bytes、stderrにwarning・診断・完了件数を出す。stdio pipeは所有するdup fdをnonblocking AsyncFdとして扱い、各行16KiB以下、通常/強制終了時に元flagsを戻す。stderr/stdoutを同じofdへ向けた場合も両方の元flagsを先に取得する。出力失敗では後続を開始しない。
- Ctrl+Cの1回目はコアへ取消、2回目は強制終了130。signal taskは終了時にabort/joinする。失敗/Unknownは1、入力不正2、Ctrl+C130、成功/warningのみ/全件ignoreは0（Phase 1時点。Phase 2では対象0件・全件ignoreを終了1へ変更した）。Unknownは診断へ明示し、成功・未変更と断定しない。停止原因が最初のCtrl+CならUnknownでも130、先に要求操作が失敗したなら後着のCtrl+Cで1を変更しない。
- endpoint注入はCLIの`test-fixtures` featureだけ。hidden `--test-endpoint`は明示config/index必須、固定dummy認証、loopback、専用namespaceで実AWSから分離する。通常buildはこの引数を拒否する。

## 結合・障害試験

全featureのRust試験はDockerを含め **71件成功**。全feature/all-target Clippy `-D warnings`、fmt、通常build/checkも成功。virtual clockで確認済みコピーが44秒後に返り、index/表示/manifestが遅延しても最初の取消から50秒までで戻ることを確認した。

実CLIの制御HTTP試験は`tests/s3/faults.py`の15項目が成功:

| 観測 | 結果 |
| --- | --- |
| HEAD 403 / 404 / 500 | 各1回のみ、本文なし、詳細とrequest ID、非ゼロ終了 |
| 後半LIST失敗 | 全対象解決前なので本文/Copy 0回 |
| GET短縮 / FULL_OBJECT checksum不一致 | 元local宛先保持、一時領域削除、成功出力なし |
| Copy HTTP 200 + Error | 成功扱いせず、再送なし |
| 変更要求の応答喪失 | Unknown・終了1、成功出力/再送なし |
| index破損 | 元の破損bytesを保持、warning後に本文コピー成功 |
| 元属性不明/不正 | Attributes warning、本文は成功 |
| 2番目の本文で失敗 | 先行成功のファイル/出力を保持、既存の失敗宛先を保持、3番目の本文0回 |
| part失敗・Abort失敗 | 自分のIDだけ1回Abort、元失敗を保持、後始末warning、Completeなし |
| part/Abortとも応答しない状態でSIGINT | **45.012秒で終了130**、自分のAbort 1回、Complete/成功出力なし |
| 単一PUTの応答待ち中にSIGINT、そのまま応答喪失 | **30.013秒でUnknown・終了130**、送信1回、成功出力なし |
| stdout pipe切断 | コピー確定後にエラー、宛先indexの成功記録は保持 |

実DockerのCLI試験で、再帰の末尾slash配置・深さ1・設定ignore・空指定の全置換・local/S3上書き・宛先ignore時GETなし・ignore済みsymlink・symlink/FIFO拒否を確認した。1MiBでは3方向と各取得のSHA256/mtime ns/modeが一致した。[機能8項目と1MiBの生データ](validation/phase1-functional.json)を保存した。

```bash
cargo test --workspace --all-features --locked -- --include-ignored
cargo clippy --workspace --all-features --all-targets --locked -- -D warnings
cargo fmt --all -- --check
cargo build -p private-archive --features test-fixtures --locked
python3 tests/s3/faults.py --binary target/debug/private-archive --report /tmp/archive-faults.json
uv run --script tests/s3/acceptance.py --binary target/debug/private-archive --functional --sizes-mib 1 --report /tmp/archive-functional.json
python3 tests/cli/wrapper.py
```

Rust/CLI/Docker試験のためだけのPython、uv、Dockerは製品実行時の依存ではない。

## 100GB / 100GiB の制御経路

本番UploadPlan / CopyPlan / PartScheduler / completion guardに、metadataだけを返すfake part reader/senderを接続した。100,000,000,000 / 107,374,182,400 bytesについて、upload 5,961 / 6,400部、copy 746 / 800部を全て完了させた。64bit offsetの連続性・最後の長さ・全番号・完了判定を確認。

さらに第1・中間・最終partのエラー/取消を全4組合せで試し、失敗判明後の新規送信抑止、残taskのabort/join、未完了をComplete可能にしないことを確認した。所有buffer tokenの最大は6以内、同時taskは4以内。payloadを生成・read・hash・読み捨て・送信していない。実SDKのAbort ID/応答不明は上記HTTP試験で補い、100GBの実転送やそのサイズのRSSを確認したという意味にしない。

## 実CLIの容量別計測

最終ビルドで256MiB / 1GiB / 4GiBの受入に合格した。

| 対象 | upload 秒 / MiB RSS | download A 秒 / MiB RSS | S3 copy 秒 / MiB RSS | download B 秒 / MiB RSS | upload / copy part数 |
| --- | --- | --- | --- | --- | --- |
| 256MiB | 10.961 / 118.81 | 8.546 / 22.32 | 2.326 / 20.37 | 7.920 / 22.02 | 16 / 2 |
| 1GiB | 48.121 / 118.69 | 39.613 / 22.68 | 14.068 / 20.59 | 34.421 / 22.32 | 64 / 8 |
| 4GiB | 199.558 / 118.59 | 145.444 / 22.35 | 83.356 / 20.39 | 149.492 / 22.10 | 256 / 32 |

全12回の実CLI操作が終了0。最終の機能試験後もloopback fixtureを読み取り照会し、バケット全体のobject・未完了uploadがともに0件であることを確認した。全サイズでSHA256、mtime ns、UNIX mode、実wire bytes、各part番号/長さ/range、後始末が一致した。全ルートが512MiB以下、1GiB→4GiBの最大RSS差も64MiB以内。

| 対象 | server VmRSS観測峰値 MiB | local一時領域の最大allocated GiB | host最低空き GiB |
| --- | --- | --- | --- |
| 256MiB | 589.91 | 0.256 | 20.42 |
| 1GiB | 721.86 | 1.001 | 18.06 |
| 4GiB | 920.65 | 4.006 | 10.08 |

測定期間（UTC）: `2026-11-16T14:02:21.205864+00:00`〜`2026-11-16T14:17:46.437792+00:00`。macOS-12.7.6-x86_64-i386-64bit-Mach-O / x86_64 / rustc 1.99.1 (58cf8f9c3 2026-10-13)。実同時要求峰値は4。

fixture binary SHA256: `f80d41dd896787abf37cff58e43e3ffdaa2b2881b83b7cc607fcc9ea6135dedf`。SDK/DB版・source/Cargo.lock hash・image digest・VM disk・全partの記録は [生データ](validation/phase1-large.json)。障害試験は [15項目の結果](validation/phase1-faults.json)。

計測はloopback HTTP proxyが本文を64KiBずつ転送し、実際の要求数・part番号/range・wire bytesを記録する。CLIは`wait4`の最大RSS、serverは1秒間隔で採取した`/proc/1/status`のVmRSS、local一時ファイルのallocated bytes・host最低空き・Docker VMのused/freeも別に記録する。SDK/DB/Rust版、OS/CPU、image digest、source/Cargo.lock/binaryのSHA256、UTC日時とfixture namespaceをJSONへ残す。

入力はseed固定の疑似乱数列を1MiBずつ進め、各blockが異なる非圧縮データを通常writeで生成し、SHA256を逐次計算する。upload後に入力を削除し、各download後にhashと属性を照合して削除するため、local本文ファイルを重ねて保持しない。serverの遅延回収を含める保守的なpeakは3×対象サイズにmetadataを加え、hostに8GiB以上の余裕を確保して開始する。

最終受入は空き領域を確保しやすい4GiB→1GiB→256MiBの順で実施する。サイズ間は専用fixtureの全object/未完了uploadが空であることを確認してから、そのcompose volumeだけを作り直す。ソース、実データ、他のDocker資源へ触れない。準備中に再生成可能なこのプロジェクトのRust build cacheとTerraform provider cacheを整理し、通常CLIを`target/bin/private-archive`、fixture CLIを`target/fixtures/private-archive`へ保存した。通常binary SHA256は`e0a31c65676fccbdfa7a686015db7eaafde487b06bc5a9fbe0307e5eaee9d3ca`。双方のhashを保持したまま試験した。

```bash
uv run --script tests/s3/acceptance.py \
  --binary target/fixtures/private-archive --large --reset-fixture-between-sizes \
  --sizes-mib 4096 1024 256 --report /tmp/archive-large.json
```

## Terraform・ラッパー

Terraform 1.17.1 / AWS provider 6.73.0の定義はinit（backendなし）・fmt・validate成功。バケット/IAM/Lifecycleは[構成説明](../terraform/environments/main/README.md)のとおり。実AWSのplan/applyは未実施。キーをTerraformで作成・記録しない。

ラッパーのプロジェクト内成果物と[初回設定](CLI_SETUP.md)を用意し、dummy executableで直接実行/source・既定/別profile・引数引用・credential除去・親環境・終了コードを検証した。呼出し元のAWS変数がreadonlyでも、`env -u`で子プロセスの認証情報を除去できることを確認した。通常CLIの実`--help`もラッパー経由で起動できた。外部配置先はユーザ確認により`20240810_shell-scripts/bash/commands/private-archive.sh`へ確定し、同一ファイルを追加した。既存の他ファイル・親shellの設定は変更していない。

## 未確認範囲

実AWSのIAM/AssumeRole/Deep Archiveの保管・課金・復元・Lifecycleの経過動作/TLS通信は未確認。Docker成功をこれらの成功に読み替えない。100GBの実本文・転送時間・そのサイズのSDK全体RSS、ARM Mac/Linux実機、release buildの容量実測は未確認。GUI・残CLI・S3インデックス同期はPhase 2/3の範囲。

# P1-06〜P1-08 コピーコア・ローカルインデックス検証

本書はP1-06〜P1-08完了時点の記録。後続の属性・CLI・取消・大容量受入は2026-11-16に完了し、現在の結果は[Phase 1最終検証](PHASE1_VALIDATION.md)に記録する。以下の未実施・残作業は本書記録時点を示す。

2026-11-15。P1-06（S3間コピー）、P1-07（SQLite）、P1-08（3方向cpのコア接続）を順に実装・検証した。[設計](archived/DESIGN.md)・[転送計画](archived/TRANSFER_VALIDATION.md)に従う。CLI本文実行・属性保持・CLI込みの取消受入・大容量実測は後続タスク。

## S3間コピー（P1-06）

`S3Store::copier`がsyncの`RemoteCopy`を実装する。SDK型はinfra内に保持し、S3読取・保存と同じ認証・namespace・送信1回の設定を再利用する。

- 16MiB以下はCopyObject、超過分は128MiB単位のUploadPartCopy、最大4要求。コピー元本文のGET・ローカル迂回は行わない。
- 100GB / 100GiBは746 / 800パート。64bit offset・連続範囲・最終長・10,000上限をplannerで検証した。本文生成・送信を伴わない計算検証であり、本番scheduler全体の100GB相当試験はP1-15に残す。
- CopySource headerを一度だけpercent encodeし、literal `%`・連続slash・dot segment・`?`・`#`を保持する。HTTP応答制御試験でwireを確認した。
- CopyObject / UploadPartCopy / Completeの解析済みSDK結果を使う。HTTP 200内のError XML・欠けた結果・応答喪失を成功としない。SDK再送なし。エラーにコピー元・宛先・code / status / request IDを保持する。
- part失敗・取消では新規partを止め、発行済み要求を最初の停止から30秒まで待ち、今回のupload IDだけを最大15秒でAbortする。後始末の診断も有界。CopyObject等の応答喪失はUnknown。応答確認済みの単一コピーは後着の取消で取り消さない。
- コピー後に対応する宛先HEADを1回行い、サイズを照合する。同じalgorithmかつFULL_OBJECTならchecksumも比較し、不一致はIntegrity。COMPOSITE・不明・欠如はsizeのみとverification warning。実際に得た観測を`SaveOutcome.observed`でコアへ返すため、インデックス用に同じHEADを重ねない。

Docker SeaweedFS 4.53での製品adapterのコピー結果:

| bytes | コピー方法 | copy part数 | 独立した内容比較 |
| --- | --- | --- | --- |
| 0 | CopyObject | 0 | SHA256一致 |
| 1,048,576 | CopyObject | 0 | SHA256一致 |
| 16,777,217 | multipart copy | 1 | SHA256一致 |
| 134,217,745 | multipart copy | 2 | SHA256一致 |

SeaweedFSのopaque key・checksum種別の制約は[既存S3検証記録](S3_ADAPTER_VALIDATION.md)と同じ。実AWSのコピーを確認したという意味ではない。

## ローカルインデックス（P1-07）

`index`はIndexRead / IndexWrite / Reconcile / IndexSnapshotと共通型だけを所有し、SQL・rusqlite型は`infra::SqliteIndex`に閉じ込めた。rusqlite 0.43.0 / libsqlite3-sys 0.40.0をCargo.lockに固定し、実行時に同梱SQLite **3.54.1**を確認した。bundled / backup / hooksを使用。

- schema v1 / PARC application ID。主キーはnamespace・scheme・authority・raw key BLOB。sizeはu64→i64を検査し、範囲外を丸めない。未知値はNULL、元時刻の秒+nanosと1970年以前の時刻も保持する。
- 新規DBは同じ親の一時DBでtransactionにより初期化し、close後にhard linkのcreate-if-absentで公開する。同時作成時は公開済みの完全なDBを開く。既存空ファイル・不正header・別アプリ・未知schemaはエラーとして保持する。
- 専用worker・容量16のqueue、WAL、cache 8MiB、mmap無効、TEMPはFILE、busy待機2秒。通常は1接続、snapshot時も最大2接続で、設計の最大3以内。明示closeとdropでworkerをjoinする。
- exact / raw prefixの半開区間 / keyset cursor、1ページ最大256件・512KiB。除外だけのページもcursorを進める。rootsは記録から導出し、最後のobject削除で消える。queryは照会時刻を更新せず、S3を呼ばない。
- MetadataPatchは未照会・既知・照会したが不明を区別する。LISTで省略された元属性は同じ版と確認できる場合だけ保持し、版変更時は不明に戻す。入力metadataの上限は128KiB。
- 範囲照合はTEMP seenと開始revisionを使う。取得済みupsertは逐次commitし、finish時だけ未観測・開始後未更新の記録を削除する。scope / depth / ignoreを適用し、部分走査のcancelやdropでは削除しない。放棄されたsessionはleaseで検知し、queueが詰まっていても次の要求前にTEMPを解放する。
- ReconcileControlで削除のページ間にも取消可能。snapshotはBackup APIを256ページずつ進め、10秒のdeadlineとquick_checkを使う。稼働中のmain DBだけをcopyしない。受け取るsnapshotは別ファイルとして検査するだけで、共有DBを差し替えない。

9件の検証で、100GB整数・既知/不明値・元時刻とchecksum・600件を超えるページング・特殊文字prefix・ignoreのみのページ・同じ版/別版のpatch・部分走査・ignore/深さ/同時upsertの保持・最後のroot削除・破損/未知版の保持・snapshot再読込・2プロセスの同時初期化/query/write・2秒busy・取消/drop/closeを確認した。

## 一時manifestとコア（P1-08）

`CopyEngine`はLoader / Saver / RemoteCopy / PlanStore / IndexWrite / OperationObserverを受け取る。IndexReadを実行判断に使わず、管理DBが壊れていても実ストアの観測を直接使う。

1. 全コピー元を逐次列挙し、配置・深さ・両側ignoreを解決する。
2. 残った各コピー元・対応する宛先を実ストアで照会し、観測のindex反映を試みる。NotFoundだけを未作成扱いにする。
3. `ResolvedCopy`を別ファイル・別接続・別workerのSQLite manifestへ1件ずつ保存する。全対象の照会と衝突検査を終えてsealするまで、GET / PUT / Copyを開始しない。
4. manifestを逐次読み、1ファイルずつ3方向の転送を実行する。最初の要求操作失敗で新規ファイルを止め、確定成功を戻さない。index異常は1回warningを出して当該操作では無効化し、本文転送を続行する。
5. 確定成功のindex反映を試みてから結果を1件ずつobserverへ渡す。結果出力失敗では後続を開始せず、成功したストア変更・index反映は残す。

manifestは0700の専用temp directoryと0600のDB、cache 8MiB、FILE TEMP、容量16のqueue、1レコード128KiB上限。remoteの宛先identityには一意制約を置く。local宛先は最も近い既存親のdevice/inodeで対応する一時検査領域を使い、宛先と同じFS上に空の名前だけを作って、case / Unicodeの同一視やfile/dir重複を検出する。実際の本文・配置先の親ディレクトリはこの検査では作らない。検査名と削除候補もdisk上へ記録し、全件をメモリへ集めない。

private disk recordのローカルpathはOS bytesとして保存する。non-UTF-8をlossy変換しない。現在のmacOSのFSが受け付けないbyte列は転送前のネイティブエラーになり、受け付けるFSではそのまま扱う。wire形式の往復も検証した。

manifest / 検査領域は明示discardで削除する。検査名の後始末は最大3秒・128件ずつで進め、完了できない場合は残ったmanifestの場所を診断に含めて保持する。コアは後始末失敗をwarningとして元の成否を保持する。自動再開・強制終了後の自動探索は行わない。

7件のコア障害検証と4件のmanifest検証:

| 観測対象 | 結果 |
| --- | --- |
| 3方向と既存local宛先 | 実localファイルとfake storeで内容一致、remote観測をindexへ反映 |
| 最後の方のstat失敗 / 宛先のcase衝突 | 本文開始0回、配置先作成なし |
| manifest保存失敗 | 要求操作エラー、本文開始0回、discard実行 |
| index初期化 / write失敗 | warning 1回、全ファイルのコピー成功 |
| 最初の転送失敗 / 結果出力失敗 | 後続ファイル開始0回、確認済みの成功とindex反映を保持 |
| 両側ignore / 配置不能なremote key | skipまたは入力エラー、本文開始0回 |
| 成功確定直後の取消 | 成功の出力とindex反映を試み、次ファイルを開始しない |
| manifestの状態・件数 | seal前の読取拒否、600件を逐次往復、discard後の読取拒否 |
| remote重複 / local親子衝突 / Unicode同一視 | 実際のFSの規則で検出、検査用の名前を削除 |

Dockerではさらに、**実S3 adapter + 実DiskPlan + 実SqliteIndex + CopyEngine**で、1,048,593 bytesのlocal→S3、再帰S3→S3、再帰S3→localを実行した。各1ファイルの成功通知、local既存宛先の上書き、source/destinationのindex観測、SHA256一致を確認した。CLIからはまだ呼び出していない。

## 再現と最終結果

Dockerを起動してプロジェクトrootから実行する。

```bash
cargo test --workspace --all-features --locked -- --include-ignored
cargo clippy --workspace --all-features --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

**66件成功**（既存39件 + 今回27件）。内訳はconfig 4 / URI 1 / S3・進捗20 / コア7 / local 11 / manifest 4 / SQLite 9 / CLI起動2 / selection 8。Clippyと書式チェックも成功。Docker不要の通常試験は`cargo test --workspace --locked`。

macOS 12.7.6 / x86_64 / Rust 1.99.1。SQLite・S3を含むtest executableの`otool -L`はOS標準のSecurity / CoreFoundation / libiconv / libSystemのみで、外部SQLite等の追加共有ライブラリを必要としない。既存のaws-lc SHA3 assemblyのcompact-unwind linker警告は残るが、リンク・全試験は成功した。

試験別prefixを削除し、`rust-adapter-` / `rust-copy-` / `rust-core-`のobjectと未完了uploadが各0件であることを確認した。既存Dockerコンテナは稼働状態を維持した。

## 残る範囲

- 次はP1-09（元の日時・権限のbest effort保持）。P1-10はコア・manifest・observer・indexを含む停止後の処理と50秒全体予算の統合・受入。
- CLIの本文実行、SIGINT接続、help・終了コードの実運用はP1-11 / P1-14。現在のCLIは引き続き本文未実装の終了1。
- 256MiB / 1GiB / 4GiBのCLI転送・RSS・一時ディスク・所要時間、本番schedulerにfake senderを接続した100GB/100GiB相当の制御経路はP1-15。100GBの実転送・そのサイズのRSSは未確認。
- 実AWSのIAM / AssumeRole / Deep Archive / Lifecycle / TLS通信、ARM Mac / Linux実機、S3インデックス同期は未確認。Terraform・AWS・外部ラッパーは変更していない。

一次資料: [CopyObject](https://docs.aws.amazon.com/AmazonS3/latest/API/API_CopyObject.html)、[UploadPartCopy](https://docs.aws.amazon.com/AmazonS3/latest/API/API_UploadPartCopy.html)、[rusqlite 0.43.0](https://docs.rs/rusqlite/0.43.0/rusqlite/)、[SQLite Backup API](https://sqlite.org/backup.html)。

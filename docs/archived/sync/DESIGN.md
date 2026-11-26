# sync 設計

2026-11-22、P2-02/P3-02の具体方式を[Phase 2・3実装設計](../PHASE2_3_DESIGN.md)に確定した。以下はPhase 1からの段階的検討記録を含み、残論点の現行解決は同実装設計を優先する。
全体設計: [../../../DESIGN.md](../DESIGN.md)。Phase 1のport・所有クレート・依存方向は全体設計のP0-02に確定。具体的な転送・timeout・メモリは [転送・検証計画](../TRANSFER_VALIDATION.md) を正本とする。

## 構成方針

- CLIから独立したユースケースを提供し、ストアの具体操作を adapter に分離する。
- 読み取りと書き込みを別の責務とする。
- Tokioと有界なストリーミング処理を前提にする。

## Phase 1の設計

- URIに応じたストア実装を解決し、コアは共通の情報・本文ストリーム・操作結果を扱う。
- 再帰走査とページ取得を逐次行い、全コピー元・対応する宛先を照会・解決してから本文転送を開始する。1件ずつ転送・結果通知し、ファイル間は並列化しない。
- ignore対象は実操作から除外し、インデックスへ新規反映しない。既存レコードは保持し、再照合の削除判定にもignoreによる除外を反映する。詳細は [全体設計](../DESIGN.md) に従う。
- 全対象は管理インデックスと別のPlanStoreへ逐次記録し、正常解決後にsealする。PlanStoreの失敗は要求操作のエラー、付属のインデックス反映失敗はwarning。両者を同じDB接続や同じ成功条件にしない。
- ファイル全体を読み込まず、上限付きチャンクを読み書きする。
- S3 multipartはinfra内のadapterに置く。16MiBのuploadパート・同時4要求・待機1・組立て1、copyは128MiB単位とする。SDKを含め自動再試行は無効にする。
- 失敗・キャンセルで新規処理を止め、発行済み要求を最大30秒待ってから今回のupload IDのAbortを最大15秒試みる。付属処理を含む後始末全体は50秒まで。成否不明を成功・未変更と断定しない。後始末失敗はwarning、残存分は開始1日後からのLifecycle対象。
- local宛先は同じディレクトリの一時ファイルへ書き、期待サイズ・利用可能なchecksum・本文終端を検証してから置換する。元の日時・権限はbest effort。checksum欠如・非対応だけはwarning、不一致はerrorとする。
- 更新時は指定範囲の実データ照会、インデックス反映の試行、照会結果による実行可否判定、実操作、成功結果の反映の試行の順に進める。実ストア照会結果を直接実行判断へ渡し、インデックス保存成功を情報経路の前提にしない。
- 最初の要求操作の失敗で停止し、コアから詳細エラーを返す。更新系操作のインデックスに関する取得・初期化・読み取り・反映・保存の失敗はwarningとし、要求操作の成否を変えない。
- 成功済み操作をロールバックせず、コマンド全体の原子性とインデックスの永続的な回復保証を要求しない。
- アーカイブ状態の取得・変更はストアの能力に応じて提供する。S3の保存クラス名を共通APIに固定しない。
- 複数URIの復元は共通の復元期間・方式とURIごとの再帰・深さを入力として扱う。全URIのキー解決・重複排除を終えてから要求を開始する。対象件数に比例するメモリ保持を避けるため、ディスク上の一時領域等を候補とし、具体方式は設計時に決める。
- 事前照会で復元中・復元済み・即時取得可能と分かればエラーを返し、期限更新のための再送やwarningでのスキップを行わない。
- 通常の一覧はローカルqueryだけを使い、初回でもS3 APIを発行しない。リモートインデックスの変更確認・取得は更新系操作または再照合だけの経路に置く。破損の詳細は呼び出し元へ通知し、更新系操作ならwarning、lsの要求操作を完了できない場合はエラーとする。自動再構築しない。
- `reconcile` では実ストア取得とインデックス更新自体を要求操作として扱う。列挙途中の失敗時は取得済み反映を残してエラーを返し、未取得範囲の記録を削除しない。インデックス更新不能の場合もエラーとする。
- SlintのGUIからも指定ディレクトリ配下の再照合として同じユースケースを呼ぶ。ローカル再読込とリモート更新の経路を分け、GUIの表示・起動だけからリモート経路を起動しない。
- 結果・warning・進捗はOperationObserver、キャンセルはCancelContextへ渡し、コアから端末入出力を参照しない。キュー・最大件数・本文bufferは転送計画で制限する。

Phase 1はP1-15まで実装・受入済み。[最終検証](../../PHASE1_VALIDATION.md)を参照。以下の各実装節はその段階の記録で、当時の後続タスクはP1-09〜P1-15で完了した。

## P1-02/P1-03の実装

- `archive-domain`にURI・型付きの単一remote object・メタデータ・checksum・診断を置いた。S3キーはdecode/正規化せず、`CopyRoute`のremote objectには空キーを入れられない。
- `archive_sync::Selection`と`CopyLayout`は1候補ずつ範囲・深さ・配置先・両側のignoreを解決する。深さだけのcp指定は入力エラー。再帰では元ディレクトリ名を追加せず中身を配置する。remote→localで相対部分が絶対パス・空成分・`.`・`..`を含む場合はエラーにする。ローカル名のUTF-8変換に失敗した場合、S3キーをlossy変換しない。
- `IgnoreFilter`は指定パターンだけをコンパイルし、除外された親ディレクトリの子を否定パターンで復活させない。localはCWD、単一remoteは親prefix、再帰remoteは指定prefixに基準を固定する。CLIリストの `\,` と `\\` を先に解釈し、その他のgitignore escapeは保持する。
- `Loader` / `Saver` / `WriteSession` / `EntryCursor`はobject-safeな非同期port。具象local adapterは`archive-infra`に置く。cursorは1件ずつEntryまたは除外warningを返し、失敗後は終端となる。深さ優先走査のメモリ・ディレクトリhandleは階層の深さに比例し、全対象件数に比例する一覧を作らない。
- `LocalLoader`はsymlink_metadataで照会する。本文open時はO_NOFOLLOW/O_NONBLOCKとdescriptorの通常ファイル判定を使い、symlink/FIFO/socket/device等を本文readerとして返さない。末尾`/`で明示symlinkの検査を回避させない。
- `LocalSaver`は親を作成し、宛先と同じディレクトリに0600の一時ファイルを作る。本文pumpとTokio Fileのbufferはそれぞれ最大1MiB。サイズ・EOF・対応checksumの検証後、未完了書込をflushしてから置換する。失敗したsessionは再使用できず、明示abortでclose/removeの結果を確認する。dropはbest effortの補助とする。
- FULL_OBJECTのCRC32/CRC32C/CRC64NVME/SHA1/SHA256を逐次検証する。COMPOSITE・未知algorithm・checksum欠如はsize/EOF検証とwarning、不一致はエラー。ETagをchecksumとして使わない。
- `CancelContext`は共有取消通知とdeadlineを持つ。pumpは読取やwarning出力の待機中にも取消を受ける。失敗時はabort最大45秒と診断最大5秒を上限とし、元のエラーを保持する。置換の確認後に届いた取消は確定成功を覆さない。
- 全対象の事前照会・宛先衝突検出・一時manifest・実データ操作とindexの接続はP1-08で実装済み（後述）。通常ファイルの日時・権限の復元はP1-09。S3 adapter・CLI本文実行・100GB制御経路・RSS実測は後続タスクに残る。今回のローカル検証を3方向cpの完成としない。

実装時に参照した一次資料: [AWS CLIの配置処理](https://github.com/aws/aws-cli/blob/develop/awscli/customizations/s3/fileformat.py)、[ignore builder](https://docs.rs/ignore/0.4.34/ignore/gitignore/struct.GitignoreBuilder.html)、[Tokio fsの取消](https://docs.rs/tokio/1.54.1/tokio/fs/index.html#cancellation)、[tempfileのpersist](https://docs.rs/tempfile/3.27.0/tempfile/struct.TempPath.html#method.persist)。依存版はCargo.lockに固定する。

## 後続の決定待ち

P1-04/P1-05ではinfraに`S3Store` / `S3Loader` / `S3Saver`を追加した。[S3 adapter検証記録](../../S3_ADAPTER_VALIDATION.md)にSDK構成・認証・メモリ上限・timeout・故障応答・Docker往復の結果を記載する。SDK型はinfraに閉じ込める。`WriteSession::failure`を追加し、非同期part失敗で本文待機中のproducerを起こす。`CancelContext`は最初の取消時刻を保持し、後始末が段階ごとに延長されないようにした。cursorのEntryはBoxで1件ずつ返す。checksum種別を取得できない場合はUnknownとして扱い、FULL_OBJECTを推測しない。

## 残作業

S3インデックスの取得・書戻しとの接続はP2-02、rm/復元固有の契約はP2-01へ残す。Phase 1の詳細エラー・共通型・認証・checksum・中断・上限値は全体設計と転送計画に従う。途中再開は含めない。

2026-11-19の要件レビューで、全CLIの深さ指定は再帰併用必須・0不可と確定した。復元はURI指定順・各URI内キー昇順、重複は最初の出現に割り当て、全対象の事前可否検査で不適格対象があれば要求を一切送らない。削除・復元は1件ずつ要求し、削除はキー昇順とする。並列実行オプションは任意の改善候補として必須範囲から外す。中断・失敗時は最初の停止から全体50秒の収束予算を使い、新規要求を止め、発行済み要求の確認と確認済み成功の通知・反映を試みる。応答不明は成否不明、追加照会・自動再試行・成功分の取り消しなし。具体的な接続は設計レビューで定める。

既存の `DESING.md` は空のためそのままとし、設計内容は本ファイルに記録する。

## P1-06〜P1-08の実装

`RemoteCopy`と`PlanStore`をportに追加し、`CopyEngine` / `CopyRequest` / `CopyOutcome` / `CopySummary` / `OperationObserver`で3方向を接続した。全対象を事前に照会し、全件のmanifest保存とsealを終えるまで本文要求を開始しない。実行判断には観測metadataを直接渡し、IndexReadを使用しない。index準備/反映失敗は一度warningを出して当該操作で無効化し、manifestの失敗は要求操作エラーとする。最初の失敗で新規処理を止め、確定成功の反映と通知を試みる。

`SaveOutcome.observed`はadapterが既に取得した宛先metadataを返す。S3 copyは検証用の宛先HEADを再利用し、upload後はindexが利用可能な場合だけ対応する宛先を照会する。後者の失敗はindex warningであり、確定した本文成功を覆さない。結果は逐次awaitするためbackpressureがあり、コアで結果一覧を保持しない。

DiskPlanは管理indexと独立したDB/workerを持ち、remote identityの一意制約と同じ宛先FS上の名前検査で衝突を検出する。private manifest内のlocal pathはOS bytesを保持する。本文を保存せず、1件ずつ読み出す。詳細・66件の最終検証・未確認範囲は [検証記録](../../CORE_COPY_VALIDATION.md)。属性はP1-09、コア全体の停止/後始末予算とCLI受入はP1-10/P1-11/P1-14に残す。

## P1-10の共通停止予算

1操作ごとに新しいCancelContextを作り、最初の取消/要求操作失敗の時刻を共有する。contextのfailure時刻は終端であり、失敗後の別コマンドへ再利用しない。partの背景失敗も直ちにrecord_failureし、`ancillary`がindex/observer等の待機にstop+47秒までの上限を適用する。PlanStoreへ残りのcleanup deadlineを渡し、dropでも後始末予算を取り直さない。操作の成功を確認済みなら通知とindex反映を試み、全体の最初の停止原因をCLI終了コードに使う。CLIの最終エラー出力も同じ50秒予算に含める。検証は [Phase 1記録](../../PHASE1_VALIDATION.md) を参照。

## インデックスのファイル転送境界（2026-11-22、D-07確定）

syncは保存先portとDBのIndexSnapshot portを組み合わせて取得・反映・終了時保存を調整する。保存先portはファイルを不透明なバイナリとして扱い、取得済みファイルと同じ実行の保存セッションを返す。ETag/HTTP/SDK型をportへ公開せず、indexにも渡さない。条件付き取得/保存と転送キャッシュの管理はinfraの保存先adapterに閉じる。

DB adapterは保存先を知らずファイルを検査・読み込み・生成し、保存先adapterはDB形式を解釈しない。ローカル/S3で同じ形式を使い、手動コピーに専用変換を必要としない。通常閲覧・再読込ではremoteの取得を呼ばず、更新系・再照合だけで準備する。条件付き取得の省略結果は対応する取得済みsnapshotに限って使い、更新可否は今回の実ストア照会で判断する。


## P2-05/06の実装（2026-11-22）

ReconcileEngineはLoaderとIndexWriteを受け、IndexReadを使わず、正常に取得した範囲だけfinishする。保存先のIndexFileStore/IndexFileSessionはopen/load/save/closeを提供し、SnapshotFileのpathを不透明なファイルとして渡す。ETag/HTTP/DB型は公開せず、共有組立てはinfraのManagedIndexが担当する。通常参照は保存先portを呼び出さない。

## P2-07の全件事前検査と復元（2026-11-22）

RestoreEngineはLoader/RemoteRestore/任意IndexWriteとObjectPlanを受け、IndexReadを使わない。全URIの観測・重複排除が終わるまで要求を送らず、不適格な観測が1件でもあれば要求0件。重複は最初のURIに順序を割り当て、後で取得した情報を判定へ反映する。未知だけ追加照会し、失敗/依然未知なら要求しない。受付後の状態は観測時刻を持つRestoreAcceptedから反映し、確認済み結果の通知後に次の要求へ進む。最初の失敗/取消では新しい要求を止める。

ObjectPlanは管理indexと独立した一時的なport。infraのDiskObjectsは所有者専用の一時SQLite DB/workerを使い、identity一意制約、URI順位/キー順、seal/rewind/replace/discardを提供する。全件をVecで保持せず、1件上限128KiB。再開に使わず終了時に破棄するmanifestなのでjournal/synchronousはOFFとし、管理indexの耐久設定には適用しない。最大100万件/RSSはP2-12で検証する。

## P2-08の削除ユースケース（2026-11-22）

RemoveEngineはLoader/RemoteRemove/任意IndexWriteとObjectPlanを使う。実ストア列挙・ignore・範囲/深さ適用を完了してsealした後、1回の確認またはauto-approveを経てキー昇順でDeleteObjectを送る。破損/古いindexから対象を選ばず、正常に確認できた削除だけ記録を消す。明示再照合のfinishを呼ばず、ignore・範囲外・未取得の記録を削除しない。

最初の要求失敗/取消では新しい削除を止め、確認済み削除の反映/表示とmanifest破棄を共通停止予算内で試みる。成否不明の追加照会・再送・ロールバックは行わない。確認UIはRemoveObserver port、非TTYの拒否はCLIの入力条件とし、フロントエンドから独立したcoreに端末I/Oを持ち込まない。

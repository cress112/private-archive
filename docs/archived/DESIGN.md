# private-archive 全体設計

## 状態

要件の正本は [REQUIREMENTS.md](REQUIREMENTS.md)。2026-11-15の合意をもとにP0-02・P0-03を確定し、2026-11-16にPhase 1の全15タスクを完了した。共通設定・対象解決・3方向コピー・SQLite・独立manifest・属性・共通取消予算・実CLI・Terraform・外部認証ラッパーを実装済み。Dockerの256MiB/1GiB/4GiB受入と100GB相当の本文なし制御経路検証が成功した。詳細は [Phase 1検証](../PHASE1_VALIDATION.md)。2026-11-22に残る要件・D-01〜12・タスクレビューを完了した。Phase 2・3の具体方式は[実装設計](PHASE2_3_DESIGN.md)を正本とし、実装・実動作検証はこれから行う。
P0-02の正本は本書の「Phase 1の境界と契約」、P0-03の詳細は [index設計](index/DESIGN.md)、P0-04の詳細は [転送・検証計画](TRANSFER_VALIDATION.md) とする。後半の段階的検討メモは当時の記録を含む。Phase 2・GUIの現行の具体方式は上記実装設計を優先する。

## 確定した構成方針

- Rust の workspace として、CLI と `sync`・`index` のコアを分離する。
- CLI の引数・表示・確認処理をコアへ持ち込まない。将来の別フロントエンドからコアを呼べる構成にする。
- 今回の完成目標にSlintの独立GUIを含め、CLIと同じ設定・インデックスと共通コアを使う。階層一覧・ローカル再読込・指定ディレクトリ配下の再照合を提供する。GUIの詳細は [GUI設計メモ](gui/DESIGN.md) にも記載する。
- ストア操作は port / adapter で分離する。初期 adapter は S3、将来候補は B2。
- 読み取りと書き込みの責務を分離する。
- オブジェクトは対応する URI で指定し、登録済み対象だけに制限しない。
- Tokio と逐次処理を前提にする。
- インデックスファイルの保存先を設定で指定する。ローカルを既定とし、S3オブジェクトも保存先にできるようにする。
- インデックス技術もport / adapterで分離する。SQLite・redbの型・クエリ・保存形式をコアの公開APIへ持ち込まず、具象実装はinfra層へ閉じ込める。
- adapterの差し替えと、既存DBの別形式への移行を分けて設計する。Phase 1のローカルDBにはSQLiteを同梱して使う。DB間の自動変換は追加しない。
- インデックスの管理単位はオブジェクトとし、バケットの一覧はオブジェクト記録から導出する。最後のオブジェクト記録がなくなればルートも消え、バケットだけを管理対象として保持しない。

## 提供順とタスク

約100GBの3方向のcpをPhase 1で先に実用化し、Phase 2でCLI機能の残り・同時起動・移行検証をそろえる。Slint GUI・設定値更新UI・GUIラッパーは最後のPhase 3で追加する。各段階に要件決定・設計・実装・検証を置き、必要な検証を後段へ一括で先送りしない。詳細は [TASKS.md](TASKS.md) に記載する。

## Phase 1の境界と契約（P0-02）

### 所有クレートと依存方向

| クレート | 所有する契約・実装 | プロジェクト内の依存先 |
| --- | --- | --- |
| `domain`（実装着手済み） | URI・対象範囲・メタデータ・保存クラス・復元状態・詳細エラー・warningの共通型 | なし |
| `config`（読込・検証を実装済み） | TOMLの読込・検証・保存、有効値と出所の解決。CLI/GUI共用 | `domain` |
| `index` | `IndexRead`・`IndexWrite`・`IndexSnapshot`のport、queryと更新の意味 | `domain` |
| `sync` | データ操作のユースケース、load/save/remote copy/manifestのport、結果通知とキャンセル | `domain`、`index` |
| `infra`（local・S3 load/save/copy・SQLite・manifestを実装済み） | local/S3/SQLite/一時manifestのadapter、SDK/DBエラーの共通型への変換 | `domain`、`index`、`sync` |
| `cli` | 入力解析・標準入出力・Ctrl+C、adapterの組立て | `domain`、`config`、`index`、`sync`、`infra` |
| `gui`（Phase 3） | Slintモデルとイベント、同じコア・設定・adapterの組立て | CLIと同じ下位層。`cli`には依存しない |

矢印は上表の依存先だけに向ける。コアからinfraやフロントエンドへの逆依存を禁止する。ユースケースは`sync`に置き、applicationクレートは追加しない。`infra`の公開コンストラクタも独自設定型とport実装を返し、AWS SDKクライアント・rusqliteのConnection等を返さない。
P1-01でworkspace memberを明示列挙し、Cargo.tomlのないGUI文書ディレクトリを対象外にした。GUIの実装時にmemberを追加する。

### 共通型

- `ObjectId { namespace, scheme, authority, key }`: S3のnamespaceは`aws`、authorityはバケット、keyはUTF-8の完全一致キー。プロファイル名を識別に含めない。同じバケットを別の認証で見ても同じ対象とする。模擬S3はendpoint別namespaceを使って実S3の記録と混ぜない。独自endpointはtest-fixtures featureを有効にしたCLI検証用ビルドのadapter設定として注入し、通常設定に公開しない。
- `Location = Local(PathBuf) | Remote(ObjectId)`。ローカルの相対パスは呼び出し時のCWDで固定する。S3 URIは最初の`/`でbucket/keyを分け、key内部の連続`/`・`.`・`..`・`%`・`?`・`#`をURLとして正規化・decodeしない。`s3://bucket/a%20b`は文字列`a%20b`のキー。選択用のObjectIdは空キーでバケットルートを表し、object専用のCopyRouteには空キーを入れられないRemoteObjectを使って区別する。ARN・Access Point・version IDの選択は今回の構文に含めない。
- `Selection { root, mode: Exact | Descendants, max_depth: Option<NonZeroU32>, ignore_base }`。再帰のrootは末尾`/`の配下prefixにそろえ、キー自体は含めない。全CLIコマンドで深さだけの指定と深さ0は入力エラー、`--recursive` との併用必須とする。復元はURIごとに検証する（2026-11-19の要件確定を反映）。
- `ObjectMetadata`: identity、`size: Option<u64>`、`data_class`、`restore_state`、`restore_expiry`、`original_mtime`、`original_creation_time`、`unix_mode`、`queried_at`、store側last-modified、opaque version token、checksumを保持する。未知値はOption等で明示し、ゼロ・現在時刻で埋めない。時刻はUTCの秒とナノ秒（現在のportではSystemTimeとして保持し、DB/表示時に変換）。保存クラスは`Standard | Archived | Other(String)`、復元状態は`NotRequired | Archived | Restoring | Restored { until } | Unknown`とする。外部クラスの扱いはadapterで分かる範囲だけ分類する。
- `MetadataPatch`は未照会・既知値・照会したが不明の3状態を区別する。LISTにない元属性を無条件にNULLで上書きしない一方、同じオブジェクトの別の版へ古い属性を移さない。保持・無効化規則はindex設計に記載する。
- `OperationError { kind, stage, source_uri, destination_uri, code, message, request_id, causes, outcome }`。kindはInput/NotFound/Permission/Unavailable/Integrity/Io/Cancelled/Unsupported等、outcomeはNotStarted/Failed/Unknown。SDK・DBの具体型は外へ出さず、code・request ID・原因列で詳細を保持する。認証情報や署名URLは含めない。
- `Warning { category, stage, uri, code, message }`は付属インデックス・属性・検証不能・後始末・ignore等を区別する。要求操作のerrorと別経路にし、warning数に比例して保持しない。

### load/saveとコピー3方向

portは`Send + Sync`、非同期メソッドは`Pin<Box<dyn Future<Output = Result<T, OperationError>> + Send + 'a>>`を返すobject-safeな契約とする。これで実行時のadapter差し替えを可能にする。本文は`Pin<Box<dyn tokio::io::AsyncRead + Send>>`として借用・所有権を明示し、全本文を返す`Vec<u8>`を契約にしない。

| port（所有者） | メソッドの契約 |
| --- | --- |
| `Loader`（sync） | `stat(location)`→存在情報またはNotFound、`enumerate(selection)`→1件ずつ取得するfallible cursor、`open(source)`→期待サイズ・メタデータ・本文reader |
| `Saver`（sync） | `probe(destination)`→既存/未作成と取得可能な情報、`begin(destination, metadata, size, cancel)`→write session |
| `WriteSession`（sync） | `write(chunk)`→backpressure付き書込、`finish(verification)`→確定結果、`abort()`→終了確認・後始末。finish前のdropだけを非同期後始末の保証にしない |
| `RemoteCopy`（sync） | `copy(source, destination, options, cancel)`→サーバー側のコピー結果。非対応の組合せはUnsupported、本文のlocal迂回へ暗黙fallbackしない |
| `PlanStore`（sync） | `append(ResolvedCopy)`、`seal()`、`next()`、`discard()`。管理インデックスに依存しない必須の一時記録 |
| `IndexRead`（index） | `get(ObjectId)`、`query(Selection, cursor, limit)`、`roots(cursor, limit)`。ローカルのみ、ページ上限256件 |
| `IndexWrite`（index） | `apply(observation/result)`、`remove(ObjectId)`、`begin_reconcile` / `observe` / `finish` / `cancel`。詳細はindex設計 |
| `IndexSnapshot`（index） | 整合したDBを一時パスへexport/importする。ファイルのremote load/saveとは分離する |

`CopyRoute = Upload { local_source, remote_destination } | Download { remote_source, local_destination } | Remote { remote_source, remote_destination }`をenumで表し、local→localを公開しない。S3→S3はRemoteCopyを使い、Upload/DownloadはLoaderとSaverを接続する。routeの解決とadapterの対応能力の検証は転送前に完了する。

### cpの処理順序

1. 設定・入力・route・ignore基準を解決する。インデックスの準備は付属の試行とし、失敗しても続行する。
2. 全コピー元を列挙し、配置先を解決して両側のignoreを適用する。残った元・宛先の照会結果を`ResolvedCopy { route, source_metadata, destination_observation }`として一時manifestへ記録する。リモートの観測情報のインデックス反映も試みる。管理インデックスを読み直して実行判断しない。
3. 全範囲の解決・必要な照会が正常終了したらmanifestをsealする。失敗・一時ディスク不足・キャンセル時は本文転送を始めず破棄する。manifestは操作履歴や再開状態として永続化しない。
4. manifestを逐次読み、1オブジェクトずつ転送する。通常ファイル間の並列化はPhase 1では行わず、multipart内だけ並列化する。S3→localでS3キーがローカルの親脱出・絶対パス等にしか配置できない場合や、異なるキーが同じローカル宛先へ衝突する場合は、事前解決でエラーにする。名前をlossy変換して上書きしない。
5. 転送サイズ・可能なchecksum・本文終端を検証してからfinishし、確定成功を通知してリモート宛先等のindex結果反映を試みる。属性の取得・保存・復元失敗はwarning。最初の要求操作失敗で次のファイルを開始しない。
6. 成功分のローカルインデックスcommitを試み、manifestを削除する。終了を待ち続けず、後始末の上限と成否不明を扱う。リモートインデックス保存設定の動作はP2-02/P2-06で接続する。

新規宛先はNotFoundだけを不存在として扱う。S3 HEADの403を404に読み替えない。転送前のstatはsnapshotや将来の書込権限の保証ではなく、発行後の失敗は実操作の結果で扱う。

### 結果・通知・キャンセル

- `CopyOutcome { source, destination, bytes, verification }`の成功通知を1件ずつ渡し、summaryには件数・合計バイト・warning件数だけを集計する。全結果・全warningはcollectしない。
- `OperationObserver`は成功結果・warning・進捗を受ける非同期sinkとする。成功結果とwarningは有界キューでbackpressureをかけ、進捗は最新値へ集約できる。CLI/GUIが描画し、コアはstdout/stderrを参照しない。observerへの結果出力失敗は要求操作のエラーとして新規処理を止めるが、成功済みコピーは戻さない。
- キャンセルはsyncが所有する`CancelContext`から伝え、CLIのSIGINTとGUIのイベントを同じ要求へ変換する。workerをdetachせず、各portへdeadlineを渡す。最初のキャンセル後は新規要求を停止し、上限内で進行中要求の結果確認・session abort・インデックス反映を試みる。値は転送計画に定める。
- SDK呼び出しをdropしても、サーバー側で未実行だったとは判断しない。PUT/Complete/Copyの応答喪失はUnknownで、既存宛先不変や失敗を断定しない。Unknownを成功として出力・index反映しない。

### 検証可能な境界

P1-14ではfake portで、全対象解決前の本文要求0回、index失敗でも転送成功、最初の失敗後の新規ファイル0件、checksum不一致時のlocal置換0回を観測する。P1-07では同じIndexRead/Write契約をSQLite adapterへ適用する。依存関係とportの具体型漏れは実装時のCargo依存と公開APIで確認する。

## 後続段階の責務メモ

| 要素 | 責務 | 持ち込まないもの |
| --- | --- | --- |
| `cli` | 引数解析、設定の入出力、コアの呼び出し、確認、表示、終了コード | 転送・同期の業務判断、DB操作の詳細 |
| 一覧専用GUI | Slintによるカラム・詳細表示、ローカル再読込、選択範囲の再照合、共有設定の編集 | DB操作の詳細、S3 SDK固有型 |
| `sync` | コピー・列挙・復元・削除・再照合のユースケース調整 | CLI構文、標準出力への直接表示 |
| `index` | オブジェクトメタデータのローカル保存・検索・更新、既知ストアルートの列挙 | S3 SDK型、認証情報、CLI構文 |
| ストア adapter | プロバイダ固有の API 呼び出しとドメイン表現への変換 | フロントエンドの表示形式 |

Phase 1の所有クレート・依存方向は上記P0-02の決定に従う。後続機能もこの境界を使う。

## port の設計案

- 読み取り: オブジェクト情報の取得、一覧の逐次取得、本文のストリーム取得。
- 書き込み: 本文の逐次保存、削除。読み取りとは独立した契約にする。
- アーカイブ操作: 対応能力の照会、復元要求、状態取得。全ストアが対応するとは仮定しない。
- インデックス読み取り: 既知ルートの列挙、URI・prefix・深さによる検索。
- インデックス書き込み: 個別反映、範囲の照合。トランザクション境界は同期の失敗要件と合わせて決める。
- SDK固有のレスポンス・継続トークン等は adapter 内で扱う。
- Phase 1 のコピーの対象範囲はローカル→S3・S3→ローカル・S3→S3で確定。将来のバックエンド間コピーや非対応機能の表現方法は未確定。

### コピーの組み合わせ

上記P0-02のCopyRoute enumとLoader/Saver/RemoteCopyを採用する。初期の組み合わせはローカル→S3・S3→ローカル・S3→S3。

## データフロー案

### cp・rmのパス解釈

cp・rmはAWS CLIのパス解釈・対象選択・コピー配置に合わせる要件を満たす。特に、完全一致キーと末尾 `/` を補った配下prefixを区別する。具体例と確定した範囲は [全体要件](REQUIREMENTS.md) を参照する。
パスの解決とignore等の共通規則をコア側で適用できる構成にし、一覧専用GUIと将来のGUI拡張からも同じ規則を再利用できるようにする。Phase 1の具象型・port・責務は本書のP0-02に従い、rm等の後続機能はPhase 2で具体化する。

### 複数URIの復元入力と実行可否

CLIは全体オプションをURI列より前に、各URIの再帰・深さをそのURIの後に受け取る確定構文を解析する。復元期間・方式の共通設定と、URIごとの対象範囲を分けてコアへ渡す構成とする。解析器や入力型の詳細は設計段階で決める。
再帰の対象解決はcp・rmと同じ配下prefixの規則を使う。全URIを具体的なキーへ解決し、同じストア・バケット・キーを重複排除する。URI指定順・各URI内キー昇順、重複は最初の出現に割り当てる。全対象の取得済み実ストア情報で事前可否検査を終え、不適格対象が判明したら復元要求を一切送らない。列挙中の失敗時も要求を開始しない。事前検査後の状態変化・API失敗では即時停止し、受付済み要求は残す（2026-11-19の要件確定を反映）。全件をメモリへ収集せず、管理indexと独立した一時SQLite manifestで重複排除・順序を扱う。具体的なport接続は実装設計に従う。
更新系の事前照会結果から復元中・復元済み・即時取得可能と分かる対象は、復元APIを送る前にエラーを返す。復元済みコピーの期限変更として再送しない。
復元可否が不明な対象だけ追加照会し、それでも可否確定不能・追加照会失敗があれば、復元要求を一切送らず全体を失敗とする。通常一覧の不明値補完にこの経路を接続しない（2026-11-20の要件確定を反映）。

削除・復元は1件ずつ要求し、削除はキー昇順、復元は上記の順序とする。オブジェクト間の並列実行オプションは今回の必須範囲に含めず、現段階では実装しない。最初の中断・失敗から付属処理込み最大50秒の共通収束予算内で発行済み要求の確認と確認済み結果の通知・反映を試みる。応答不明は成否不明として返し、追加照会・自動再試行・成功分の取り消しは行わない（2026-11-19の要件確定を反映）。port接続は実装設計、停止予算は既存のCancelContext/ancillaryの50秒共通予算に従う。

### 設定とインデックス保存先

設定ファイルはTOML、既定パスは `~/.private-archive/config.toml` とする。
共有configクレートが設定を解決し、コアは解決済みの値だけを受け取る。

```toml
# Phase 1のローカル設定例。S3 indexはPhase 2で接続する。
[index]
location = "~/.private-archive/index.db"
# S3を選ぶ場合の例:
# location = "s3://metadata-bucket/private-archive/index.db"
```

`~`と相対パスは共有configローダで解決する。設定内の相対パスは設定ファイルの親、実行時指定はCWDを基準にする。詳細はindex設計に従う。
`--config PATH` で設定ファイルを変更する。未作成時は既定値を使い、作成はCLIのcfgの作成系コマンド、またはGUIの明示的な設定操作で行う。cfgの確定構文はCLI要件に従う。GUIの編集・保存・適用操作の詳細は後続で決める。

リモート保存時は、ローカル作業用DBを取得・更新し、一貫したスナップショットを保存先へ書き戻す構成を提案する。
ローカルのDB検索・更新と、DBファイルの取得・保存を別のportにする。後者のS3呼び出しはadapterへ隔離する。
取得・保存もファイル全体をメモリに読み込まず転送する。更新中のDBファイルをそのままアップロードしない。
インデックスは状態の写しであり、保存成功を実データ操作の成功条件にしない。未反映コピーの保持・廃棄は後続の設計対象であり、永続的な回復保証は要件に含めない。

`ls` はローカルインデックスのqueryだけで結果を作り、S3 APIを発行しない。S3インデックス自体の変更確認・取得も行わない。
インデックス自体の変更確認を行う場合、その対象はリモートインデックス1オブジェクトとする。
S3保存時の変更確認・取得は、更新系操作または再照合の要求時だけ実施する。再照合にはCLIの `reconcile` とGUIの指定ディレクトリ配下の更新を含める。保存済み情報の古さ自体は許容する。更新系操作での失敗はwarningとし、要求操作を止めない。再照合は要求された更新を完了できなければ操作失敗とする。
S3インデックス自体の取得/Fetchと、reconcileによる実ストアの再照合/Reconcileは区別する。再読込/Reloadはローカルだけで完結する。
書き戻しは終了時1回とし、競合は同じ実行内で通知して終える。未送信キューや実行をまたぐ専用復旧は追加しない。更新系操作ではインデックスの取得・事前反映・結果反映・保存等の失敗をwarningとして通知し、要求操作の実行判断と成否から切り離す。`reconcile` では要求された更新を完了できなければエラーとする。
インデックス保存先をデータ走査範囲内に置く場合、インデックス自身を取り込まないための除外規則も定義する。

### DB形式と保存先の分離（2026-11-22、D-07確定）

| 境界 | 所有するもの | 実装 |
| --- | --- | --- |
| indexの論理port | 検索・反映・範囲照合・整合済みファイルの入出力契約 | indexはDB形式・保存先・ETagを知らない |
| DB adapter | ファイルの読込/生成/検査、スキーマ、DB transaction、整合snapshot | infraのSQLite adapter。redb等は同じportの別adapterとして追加可能 |
| 保存先port | 不透明なファイルの取得・保存とその実行内の保存セッション | syncが契約を所有し、ファイルの中身を解釈しない。ETag/HTTP/SQLを公開しない |
| 保存先adapter | ローカル/S3のバイナリ転送、転送キャッシュ、変更検知と保存条件 | infra。S3のETag・条件付きHTTP要求はここに閉じる |
| ユースケース | 許可されたタイミングの取得/保存、DB portへの接続、失敗の扱い | sync。CLI/GUIで共用し、indexへ保存先判定を持ち込まない |

S3に置く本文は同じDB adapterが扱う通常のDBファイルそのものであり、S3専用の包み形式を作らない。正常に閉じた整合済みDBをaws s3 cp等で往復し、設定の保存先だけを変更して使用できる。転送用のETag・キャッシュ情報はファイルの外に置き、ファイル単体の利用条件にしない。DBのシリアライズ形式はadapterの技術詳細とし、domainやindexの知識にしない。

保存先adapterは、変更しない取得済みsnapshotとETagの対応付けを保証できる場合だけGetObjectのIf-None-Matchを使用し、304ならそのsnapshotを再利用する。変更済みの作業DBに古いETagを対応付けて取得を省略しない。対応情報/ファイルがない場合は、取得が許可された経路で通常GETする。更新系操作・再照合の開始時だけ取得し、通常ls・GUIの起動/閲覧/再読込からは、初回でも取得しない。

保存時は同じ実行で取得した版へのIf-Match、未作成と確認できた場合はIf-None-Match:*をS3 adapter内部で使う。取得失敗等で保存条件を確定できない場合は無条件上書きしない。競合は当該実行内で通知し、自動マージ・再試行・未送信キューを作らない。ETagをローカルで計算したファイルハッシュと同一視しない。

受信ファイルはDB adapterで検査してから共有作業DBへ反映し、開いているDBのファイルをrename交換しない。送信はDB adapterが生成した整合snapshotをストリーム転送する。稼働中SQLiteのmainファイルだけの生コピーは避け、手動移動では全利用プロセスの正常終了後の整合済みDBを使う。これはDBの整合条件であり、S3専用形式ではない。根拠: [SQLite WAL](https://sqlite.org/wal.html)、[Backup API](https://sqlite.org/backup.html)。

取得・保存失敗は更新系操作ではwarning、再照合では要求された更新を完了できなければerrorとする。実データの対象選択・可否判断は毎回の実ストア照会結果を使う。保存済みファイル・ETag・前回の同期成否をその代用にしない。キャッシュ配置・共有作業DBへの反映の具体方式は[実装設計](PHASE2_3_DESIGN.md)に確定した。

### 再照合の空範囲とignore（2026-11-22、D-04〜06確定）

CLIは `reconcile URI` の1 URIを必須とし、完全一致を既定にする。配下の照合には `--recursive` を明示する。GUIのフォルダ照合は既定で深さ無制限の再帰とする。
実ストアの有効範囲を正常に照会し、空だと確認してインデックスへ反映できれば成功とする。全件ignoreは状態確認を実施できないためエラーとし、削除照合へ進まない。通信・権限・保存失敗を空範囲の成功へ読み替えない。
ignore発生と確認済み除外通知件数をinfo、個々のパスをdebugで通知する。未走査の配下件数を推測したり、ログのために追加照会したりしない。GUIで既知バケットが0件なら照合範囲なしのエラーとし、S3 APIを発行しない。

### 通常の一覧

インデックスの検索結果を逐次取得し、CLIが表示する。引数なしなら既知のストアルートを返す。
詳細表示・復元状態の表示にも保存済みメタデータを使い、初回でもS3 APIを発行しない。
ローカルインデックスが未作成なら空で初期化できる。リモート保存設定でも、空のローカルコピーからリモート不存在を推測して書き戻さない。詳細・JSONでは最終照会日時を表示し、実ストアを照会した時刻とインデックスの取得・表示時刻を区別する。
破損検出時はadapterが把握できる箇所・原因を詳細エラーとして返す。通常の `ls` で問い合わせ不能、または `reconcile` で更新不能ならCLIが詳細を表示してエラー終了する。更新系操作は同じ異常をwarningとして通知し、要求操作を継続する。自動再構築・S3走査・空への置き換えを行わない。破損の検出手段は採用する保存技術に応じて設計する。

### 要件から定まるS3インデックス利用時の更新手順

1. 更新系操作の要求に基づいて設定されたインデックスの準備を試みる。S3の変更検知・取得方式は前述のD-07で確定した保存先adapterの方式とする。準備に失敗してもwarningを通知して続行する。通常の `ls` ではこの手順を実行しない。
2. 操作対象として指定された範囲を実ストアで列挙・照会する。
3. 取得した情報のインデックス反映を試みる。失敗はwarningとする。
4. 実ストアの照会結果で分かる実行可否を判定する。インデックスへ保存してから読み直すことを必須経路にしない。
5. 要求操作を実施し、成功したkeyの結果のインデックス反映を試みる。反映失敗はwarningとする。
6. 変更がある場合は終了時に1回、整合したインデックスをS3へ書き戻す。データ操作での保存失敗はwarningとし、要求操作の成否を変えない。再照合での保存失敗はエラー。競合時は他の更新を上書きせず、その実行内で通知して終える。反映済みローカルコピーは参照用に残せるが、未送信分の永続管理は行わない。
7. ローカルコピーを保持し、次回の取得を必要な場合だけに抑える。

最初の要求操作の失敗で停止し、自動リトライや成功済み操作のロールバックはしない。S3インデックスは終了時1回・競合は同じ実行内で通知・自動マージ/再試行なしと2026-11-20に確定した。質問19の追加指示により、未送信状態の永続管理・専用復旧は追加しない。途中失敗・Ctrl+C時も予算内で書戻しを試みる。多重実行・スナップショット・変更検知等は実装設計の方式に従う。cpの発行済み処理は上記P0-02と転送計画に従う。
取得済みインデックスのquery自体はローカルで行い、リモートインデックス自体の変更確認とは分ける。
コマンド全体を原子的にするトランザクションや、未同期のインデックスを回復する永続ジャーナル・再送キューは追加しない。

実ストアの照会結果・キー解決結果は要求操作に渡し、インデックスはそれらの付属の保存先として扱う。インデックスadapterの失敗で必須の情報経路が途切れないようにする。復元の重複排除等、要求操作自体に必要な処理は付属のインデックス更新と区別する。cpの型・一時manifest・portの接続は上記P0-02に従い、復元固有の接続はPhase 2で具体化する。

### 再照合

指定URIと再帰・深さの指定で決まる範囲の実ストア情報を取得し、ローカルインデックスに反映してから表示する。
プログラムが「全範囲」を自動で把握・照合することは前提にしない。
削除検知のため、取得できたオブジェクトの upsert だけでは足りない。
途中の権限エラー・通信失敗を「見えなかったオブジェクトは削除済み」と解釈してはいけない。
列挙途中で失敗した場合は取得済みのインデックス更新を残してエラーを返す。未取得範囲の既存記録を維持する。インデックス更新自体も要求操作なので、更新不能ならエラーとし、更新系データ操作のwarning方針をそのまま適用しない。
ローカルDBの照合はindex設計のTEMP seen表と開始revisionに従う。S3インデックス取得・書戻しとの接続は実装設計のDB port/保存先port分離に従う。
リモートDBのダウンロードだけでは、そのDBを経由しない外部変更を検出できない。

### 更新操作

指定範囲の実データの事前照会と、その後の操作結果のインデックス反映を試みる。付属の反映失敗はwarningとし、要求操作を止めない。
コアは詳細な要求操作のエラーを返し、最初の要求操作の失敗で停止する。CLI固有の表示はフロントエンドで扱う。
実データ操作とインデックス保存の成功を区別し、コマンド全体の原子性は要求しない。
指定範囲外を自動で全走査しない。

### multipartの後始末

アップロード失敗・キャンセル時は、その操作で開始したupload IDを使い中断・パート削除を試みる。CLIのCtrl+Cをコアのキャンセル要求に変換し、S3 adapterが後始末を行う構成を検討する。他プロセスのmultipart uploadをまとめて中断しない。後始末失敗は元の失敗・中断に追加するwarningとして返す。
進行中要求の停止・収束・中断の順序と待機上限は転送計画に従う。自動リトライは要求せず、完全な後始末を保証する構成にはしない。
Terraformにはバケット全体を対象とする `AbortIncompleteMultipartUpload` の日数1を含める。これは開始からの経過日数であり、進行中でも未完了なら対象となる。通常起動時にCLIがLifecycleを変更する構成にはしない。具体的なHCLとIAM権限は正式な設計段階で定める。
1日ちょうどでの削除を保証しない。0日の受理・動作には資料上の不確実性があり、1日を採用する。調査根拠は [全体要件](REQUIREMENTS.md) を参照する。

## ignoreとインデックスの記録

実ストア照会結果の対象選択とインデックスへの新規反映に、有効なignoreを適用する。除外対象は新たに取り込まず、既存レコードは保持する。検索時にも有効なignoreを適用し、解除後に既存レコードを再表示できるようにする。再照合でignoreによって列挙対象外となったことをリモート削除の根拠にしない。記録の更新・削除範囲を判定する具体方式は正式な設計で決める。

## 一覧専用GUI

Slintの独立アプリを採用し、CLIと設定・インデックスを共用する。認証プロファイル等はshell-scriptsリポジトリのGUI用ラッパーから渡す。GUI本体へ具体的なIAMロール名・AWSプロファイル名を固定しない。
GUIはインデックスの技術に依存しない読み取り契約を再利用し、Finderのカラム表示に倣ってバケット→ファイル階層を横並びの列で構成する。名前と種別を列の行に、選択対象の基本10項目を右の詳細欄に表示する。フォルダ選択で右隣に子を表示し、空は空の列とする。起動・フォルダ選択・ローカル再読込ではS3を自動照会せず、本文・サムネイルも取得しない（2026-11-21確定）。ローカル再読込は明示操作だけで、URIで選択階層を保持し、消えた場合は残る親へ戻す。画面状態を起動間で永続化しない。
選択範囲の再照合は、CLIの `reconcile` と同じコアのユースケースを呼ぶ。取得・更新を完了できなければ操作失敗を通知し、取得済みの部分反映を残す。トップレベル更新はインデックスの記録済みバケットを列挙し、それぞれを起点として配下を更新する。未登録ディレクトリのURI直接入力・AWSからのバケット自動発見は要求しない。
バケットが未記録の場合、GUIのトップレベル更新に対象はない。CLIで最初の取り込みを行い、GUIはローカル再読込で記録を表示する。バケットの自動発見のためにIAM権限や走査を追加しない。
表示のignore基準は表示起点URIに固定し、展開・選択では変更しない。再照合は指定ディレクトリを基準とする。トップレベルの表示・更新では各バケットルートを基準とし、バケット名を照合相対パスへ含めない。
設定値更新UIも今回提供し、共有configクレートの読み取り・値の検証・保存をcfgとGUIから再利用する。設定操作だけでS3照会・インデックス移行を実行しない。共有の型付き変更API・安全な保存・次操作からの適用は実装設計に従う。
2026-11-21、設定作成・保存・キャンセル・項目を既定値へ戻す操作、起動引数だけの一時上書き、保存は次の操作から適用・起動引数の優先を維持、共有設定の明示再読込と保存先変更時のルート表示を確定した。GUIのリモート照合は単一選択・同時に1操作で、ファイルは1件、フォルダは配下とする。キャンセル可能で、閉じる操作も確認なしのキャンセル後に最大50秒の収束予算で終了する。CLIサブコマンド・起動引数はD-01〜03で確定した。
大量のキーをUIスレッドで一括取得・構築せず、ノード展開に応じた段階取得・ページ取得等を検討する。具体的なクレート構成、モデル、UI部品、イベントループと非同期処理の接続は要件確定後に設計する。

## CLI出力の分離

コアから受け取った結果はCLIがstdoutへ、warning・エラー・進捗はstderrへ出力する。JSON出力時のstdoutにはJSONだけを出す契約とする。コアは標準入出力へ直接書き込まず、将来のGUIでも結果・診断・進捗を利用できるようにする。通知は上記OperationObserverを使い、描画は各フロントエンドで行う。
JSON配列の逐次出力中に要求操作が失敗した場合、stdoutが利用可能なら出力済みの結果を配列として閉じる。エラーはstderrに出し、非ゼロの終了コードで部分結果と正常完了を区別する。書き込み不能や強制終了時のJSON完成は保証しない。2026-11-19に全CLIの終了コードを0/1/2/130で統一し、rm確認のyes以外・EOFは再入力なしで1と確定した。削除成功URI、復元受付URI・日数・方式は逐次stdout、進捗はstderrが端末の場合だけとする。JSONの具体表記・終端処理はP2-01で決める。2026-11-20にcp・ls・rm・restoreの対象0件（全件ignore・空ls含む）は1、ログは既定info・再帰の個々のignoreはdebug、設定logging.levelと--log-levelの上書きを確定した。進捗はstderr端末かつinfo有効時のみ。型付きDiagnosticと非同期sinkで接続する。詳細は実装設計に従う。
ls詳細・JSONの基本10項目は [全体要件](REQUIREMENTS.md) に従う。未取得値を不明として表現できる、DB固有の型に依存しない読み取りモデルを使う。表示のためのリモート補完は行わない。
2026-11-20にCLI一覧の既定はURI昇順（大文字小文字を区別・言語設定に非依存）、通常1行1URI、詳細・JSONは基本10項目と確定した。prefix・ルートのサイズ等を配下の合算で埋めない。結果JSONは全体要件のsnake_case全10キーを省略せず、未知値null・UTC RFC3339と小数秒保持・サイズのバイト整数・権限の8進数文字列とする。復元状態はnot_required/archived/restoring/restored/不明null、保存済み期限超過だけでは記録を書き換えないと確定した。JSON提供は今回lsだけと確定した。表示型・serde_jsonのストリームwriterの接続は実装設計に従う。
rmの確認は実対象の列挙・ignore適用後、削除開始前にコマンド全体で1回行う。URI・件数・合計サイズを表示し、yesのみ受け付ける。確認のために全対象をメモリへ集めず、集計・対象保持の方式は設計段階で決める。
CLIは事前に `--auto-approve` と標準入力の端末判定を行う。auto-approveがなく標準入力が非TTYなら、承認を読まずリモート照会前にエラーとする。Rust標準の `std::io::IsTerminal` による判定を候補とし、パイプ排除のためのreadタイムアウトに依存しない。auto-approve時は承認読み取りを行わない。stdoutが端末であることは要求しない。
根拠: [Rust IsTerminal](https://doc.rust-lang.org/std/io/trait.IsTerminal.html)。

## 使用メモリを制限する設計案

- 一覧・検索はページまたは `Stream`、転送本文は非同期のチャンク読み書きとして扱う。
- producer が consumer を追い越し続けないよう、キューを有界にして backpressure をかける。
- 同時オブジェクト数、multipart の同時パート数、各バッファサイズにそれぞれ上限を設ける。
- DBキャッシュや一覧ページも含めてメモリ予算を管理する。全件の `collect` を処理開始の前提にしない。
- メモリ見積もりは概ね「同時転送数 × 転送あたりのバッファ上限 + キュー上限 + ページ・DBキャッシュ上限」とする。
- 同期DB処理や重いCPU処理は専用worker等へ分離する。無制限に blocking task を生成しない。
- Phase 1のmultipart・キュー・メモリ・後始末上限は転送計画に定める。途中再開は含めない。
- 実装の検証では、単一ファイルサイズと対象件数をそれぞれ増やし、設定した上限でメモリが抑えられるか確認する案とする。

## 実行時依存と移植性

pure Rust自体は必須ではなく、設定・インデックスを移行し、CLIを対象向けに再ビルドすれば追加の実行時依存なしで動くことを要件とする。
静的に取り込むC等のコードも候補になりうる。DBはbundled SQLiteを採用する。S3のTLSはrustlsを使い、暗号実装をビルド時に取り込む構成とし、P1-04で実際のリンク依存を確認する。
OS標準の共有ライブラリ・ランタイムへのリンクは許容する。CLIの対象は現在のx86_64 macOSとDockerエミュレーションのLinux ARM64を実行検証、ARM macOSを互換性調査とする（2026-11-20）。Linux ARM64はDebian系glibc環境でnativeビルドし、検証用DockerへOS標準ライブラリと必要なGUI依存を含める。DB/TLSは既存のbundled SQLite/rustlsを維持する。
一覧専用GUIおよび将来のGUI拡張のプラットフォーム依存はコアから隔離する。GUIも現在のx86_64 macOSと将来のARM macOS・ARM Linuxへの移行を対象とする。Slintのバックエンド・レンダラはD-08でWinit＋CPU描画と確定。ウィンドウ環境・採用版/features・実行時依存を具体化する。今回の受入は現在Mac実動作とDocker Linux ARM64の起動・描画・基本操作、ARM Macは公式資料と採用依存関係の互換性調査とする。ARM Mac実機・クロスビルドは必須にしない。基盤の予備確認は [移植性検証](../PORTABILITY_VALIDATION.md) を参照。

## AWS認証の現状と設計上の保留

- 読み取りのみで確認した `~/.aws/config` には `admin`・`dev` があり、いずれも `default` を引受元として MFA を設定している。
- 秘密の認証情報・具体的な識別子は設計書へ転記しない。
- 専用ロールを作るだけで MFA 要否は決まらない。引受元の権限・信頼ポリシー・関連ポリシーによる。
- 具体的なロール・プロファイルの選択は外部のBashラッパーが担い、アプリはSDKの標準認証経路を使う。
- GUIも同じ方針とし、shell-scriptsリポジトリへGUI用ラッパーを追加する。具体的なコマンド名・配置の分割・バイナリ名は後続で設計する。閲覧時に認証のためのクラウドアクセスを開始する構成にはしない。
- Rust SDKは `AWS_PROFILE` と共有プロファイルのAssumeRoleに対応する。IAMユーザ・キーの管理とTerraformの詳細は要件確定後に設計する。
- 任意URIの操作機能と、実際に認証情報が許可する範囲は独立している。

## 調査済みのS3仕様

- Deep Archive の復元では元の保管クラスを維持したまま一時コピーを期限付きで利用する。
- 復元済みコピーの期限は `RestoreObject` 再発行で更新できる。復元処理中の期限変更はできない。
- `ListObjectsV2` では `OptionalObjectAttributes=RestoreStatus` を指定して復元状態と期限を取得できる。全件の状態更新で常に個別HEADが必要とは仮定しない。
- 再アーカイブ専用操作 と復元コピーの解放・保持期限短縮の専用機能は要件から除外された。通常のオブジェクト削除で代替しない。
- S3へのインデックス書き戻しでは、取得時のETagを条件とする `If-Match` や、新規作成時の `If-None-Match: *` で競合を検出する方式をD-07で採用する。ETagは保存先adapterの外へ持ち出さず、DB形式も変更しない。競合の自動マージは行わず、その実行内で通知して終える。未送信分の永続管理・専用復旧は追加しない（2026-11-20、質問19の追加指示）。

根拠:

- [Working with archived objects](https://docs.aws.amazon.com/AmazonS3/latest/userguide/archived-objects.html)
- [RestoreObject](https://docs.aws.amazon.com/AmazonS3/latest/API/API_RestoreObject.html)
- [ListObjectsV2](https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListObjectsV2.html)
- [AssumeRole](https://docs.aws.amazon.com/STS/latest/APIReference/API_AssumeRole.html)
- [S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)
- [TOML specification](https://toml.io/en/v1.0.0)
- [GetObject: conditional requests](https://docs.aws.amazon.com/AmazonS3/latest/API/API_GetObject.html)
- [S3 object integrity and ETag](https://docs.aws.amazon.com/AmazonS3/latest/userguide/checking-object-integrity-upload.html)

## レビュー完了と判断の境界

P2-01/P3-01の利用者契約、D-01〜12、P2-02/P3-02の[詳細設計](PHASE2_3_DESIGN.md)を確定した。残る20タスクは[TASKS](TASKS.md)の順で進める。製品の実装・検証は未完了であり、設計の完了と混同しない。内部の型名・ライブラリpatch版・同じ契約内の互換性修正は実装時に判断できる。利用者向け契約・完成範囲・費用条件を変える必要が出た場合に相談する。希望バケット名が利用不能の場合は、D-11の指定どおり停止して再考する。

## 技術調査メモ（2026-11-15、採用判断は設計段階）

- DBの候補は比較材料であり、採用要件そのものにはしない。DBを選んだ後も、コアは技術非依存の契約だけを使う。
- SQLiteはOS・32/64bit・endiannessをまたいで使えるファイル形式を公式に定義している。`rusqlite` のbundled構成では同梱ソースをビルドできるため、追加のSQLite共有ライブラリを実行時に要求しない構成が候補になる。Cコンパイラ等のビルド条件は別に確認する。[可搬性](https://sqlite.org/onefile.html)、[bundled](https://github.com/rusqlite/rusqlite)
- SQLiteのWALは複数readerとwriterを同時に扱えるが、writerは同時に1つ。WAL等を伴う稼働中DBのmainファイルだけを無条件にコピーしてS3へ保存する構成にせず、整合したスナップショットの生成をadapterの責務として検討する。[WAL](https://sqlite.org/wal.html)、[Backup API](https://sqlite.org/backup.html)
- redb 4.6.0の公式APIにはExclusiveWriter・SingleWriter・MultiWriterがあり、Linux・Apple等で複数プロセスに対応する。既定のExclusiveWriterのままでは別プロセスを排除するため、同時実行の要件を満たすモードとバージョンを確認する。古い情報だけで「redbは複数プロセス非対応」とは判断しない。[ConcurrencyMode](https://docs.rs/redb/4.6.0/redb/enum.ConcurrencyMode.html)
- DB内部の短い排他・snapshotと、実データ操作全体の排他・状態管理は別物として扱う。後者を新たな必須機能にしない。
- S3 LIST・multipart・SDKリトライ・IAM等の外部制約は [全体要件の追加調査](REQUIREMENTS.md) に記載した。具体的な実装方式は確定設計と[S3 adapter検証記録](../S3_ADAPTER_VALIDATION.md)を参照。

## 最終AWS検証（2026-11-22、D-10〜12確定）

[最終AWS検証計画](../FINAL_AWS_VALIDATION_PLAN.md)に従い、Deep Archiveの小さい1個を復元後の取得まで確認する。実施はP3-11のみ・追加費用合計1 USD以下。バケット名はcress-archived-dataとし、利用不能なら停止して再考する。試験後はバケット内を空にし、バケット・ロール・ユーザと同じ運用IaC/stateを残す。現在は計画作成だけで、AWS操作は行っていない。

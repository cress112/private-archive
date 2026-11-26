# Phase 1 大容量転送・認証・検証計画

全体: [要件](REQUIREMENTS.md)・[設計](DESIGN.md)・[タスク](TASKS.md)。P0-04で確定した設計と受入計画。2026-11-15のユーザ指示により、検証はDockerのS3互換サーバーを使い、100GB実転送・実AWS費用・100GB用ディスクを完了条件から外した。100GBを扱う製品の設計目標は維持し、縮小実測と本文なし制御経路検証を分ける。

2026-11-16、Phase 1の実装と本計画の受入を完了した。実CLIの256MiB/1GiB/4GiB全12操作、内容・属性・パート・後始末が成功し、最大CLI RSSは118.81MiB。100GB/100GiBのplanner/scheduler・停止・buffer上限も本文生成なしで確認した。[最終結果と生データ](../PHASE1_VALIDATION.md)を参照。実AWSへの適用・100GB実転送は未実施。

SDKのHTTP/TLSはhyper・rustls・aws-lc-rsを使い、SQLiteもビルドへ同梱する。現在のx86_64 macOSで追加DB/TLS共有ライブラリなしをリンク先で確認済み。[SDK HTTP構成](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/http.html)

## 転送サイズとmultipart

設計対象は100,000,000,000 bytes（100GB）。100GB実ファイルの転送は今回行わず、実転送の受入は256MiB・1GiB・4GiBのDocker試験で行う。上限の計算では100GiBも扱えることを確認する。設定を利用者に増やさず、Phase 1では以下をadapter内部の既定値にする。

| 項目 | 設計値 |
| --- | --- |
| 同時オブジェクト数 | 1。ファイル間の並列化は行わない |
| 単一PUTの上限 | 16MiB。ゼロ長も単一PUTで扱う |
| multipart upload | パート16MiB、同時要求4、待機パート1、producerの組立て1 |
| multipart copy | 16MiB以下はCopyObject。それ以上は128MiB単位、同時要求4 |
| ローカルI/O・ダウンロードのbuffer | 1MiB、最大2段。全本文のcollectなし |
| LIST | MaxKeys=1000、先読みなしで1ページずつ処理 |
| DB query | 256件かつ512KiB以下/ページ |
| manifest/DB request queue | 16件、1要求は256レコード・512KiB以下 |
| 結果/warning queue | 16件、各メッセージ16KiB以下。過長な診断詳細は切詰めた旨を付す |
| 完了パート情報 | 最大10,000件、合計4MiBを上限。本文は保持しない |

100GBは16MiBで5,961パート、100GiBは6,400パート。128MiBのcopyでは各746/800パートであり、10,000パート以下。パート数・サイズ・最後の長さを開始前に検査する。受入範囲を超える入力でも、現在の固定サイズで制限を超える場合はCreateMultipartUpload前にUnsupportedとして失敗し、メモリ上限を無視してパートを大きくしない。
根拠: [S3 multipart制限](https://docs.aws.amazon.com/AmazonS3/latest/userguide/qfacts.html)。

ローカル入力を逐次読み、16MiBのBytesを作ってUploadPartへ渡す。ByteStream::from(Bytes)の共有参照を使い、再試行用に別の本文コピーを持たない。最大4要求+1待機+1組立ての6パート以外を生成しない。パートエラーが判明した時点でproducer・次パートの開始を停止する。全体のmultipartメタデータだけはS3の固定上限10,000に収まる範囲で保持する。
SDKに入力本文の再読込や再試行を任せるためのファイル丸ごとのバッファは作らない。公開ByteStreamの契約と実際のピークメモリをP1-05で確認する。[ByteStream](https://docs.rs/aws-sdk-s3/latest/aws_sdk_s3/primitives/struct.ByteStream.html)

## 使用メモリの予算

CLIプロセス単体のピークRSSを512MiB以下とする設計目標。OSページキャッシュ・別プロセスの模擬S3・コンパイラはCLI RSSと分けて測る。

| 消費先 | 予算 |
| --- | --- |
| multipart本文6パート | 96MiB |
| SDK・HTTP・TLSと転送時の一時保持 | 128MiB |
| DB最大3接続のpage cache | 24MiB |
| manifest/DB requestとTEMP cache等 | 32MiB |
| LIST/queryの現在ページとメタデータ | 16MiB |
| 本文I/O・checksum・完了パート表 | 16MiB |
| 通知キュー・worker等 | 16MiB |
| Rustランタイム・アロケータ・その他と余裕 | 184MiB |
| 合計 | 512MiB |

これは設計予算であり、RSSをライブラリの設定だけで保証したと主張しない。256MiB・1GiB・4GiBの同一経路を比較し、全試験で512MiB以下、1GiBから4GiBへのピーク差64MiB以下を受入目標にする。warm-upの差も記録する。100GB相当は下記の制御経路試験で別に検証するが、100GB実転送のRSS・所要時間を実測済みとはしない。失敗時はbuffer/SDKの保持を調べる。

## 全対象の事前解決と一時ディスク

管理インデックスと別の接続・別ファイルのSQLiteを一時manifest adapterに使う。IndexRead/Write経由ではなくsyncのPlanStore portを実装するため、管理DB破損でも必要な対象一覧を作れる。
コピー元・解決後宛先・サイズ・事前観測を1レコードずつ保存し、配置先identityには一意制約を置く。名前の正規化や大小文字非区別のローカルFSで衝突する可能性も事前解決で検査し、黙って同じファイルへ重ねない。全範囲を解決してsealするまで本文転送しない。一時manifestに途中再開機能を持たせない。

- manifestはOSのtempディレクトリ、権限0600の専用一時ファイル。空き不足・書込不能は要求操作失敗。異常な入力や過長なレコードは明示エラーとし、メモリへfallbackしない。
- TEMP領域もFILE-backed、cacheは有界。入力・宛先の件数Nに比例するディスクを許容し、通常レコードの見積りは1件16KiB+索引、実装上限は1件128KiB+索引。100万件の通常見積りでも約16GiB+索引になりうるため、実際の件数・manifest bytesを記録し、上限の大きいレコードを通常見積りで扱わない。local宛先では同じFS上の空の名前によるcase/Unicode衝突検査にも件数に比例するinodeと一時diskを使い、削除候補はdiskへ保持する。件数規模の最終受入はP2-01/P2-12で確定する。
- uploadは本文の一時ファイルを不要とし、manifestだけを使う。S3→S3も本文を保存しない。
- downloadは宛先と同じディレクトリに専用一時ファイルを作り、成功後だけrename置換する。既存100GB宛先の上書きでは元ファイルと一時ファイルを同時に保持するため追加約100GBが必要。通常の新規取得は一時ファイル約100GBが必要。
- 全対象解決が失敗したらmanifestを削除する。本文の失敗・キャンセル時は今回の一時ファイルを削除し、失敗はwarning。強制終了後の自動発見・自動削除・再開は追加しない。

## 内容検証と属性

| 方向 | 実行時の検証 |
| --- | --- |
| local→S3 | 単一PUTは本文のCRC32を付ける。multipartは各パートCRC32と、読込順に計算するFULL_OBJECT CRC32を使い、Complete時にサイズと全体CRC32を渡す。S3/SDKが返す失敗を要求操作のエラーとして扱う |
| S3→local | GETのChecksumModeを有効化し、返された対応可能なchecksumを本文と照合する。本文を最後まで読み、期待サイズと終端検証に成功してから置換する |
| S3→S3 | Copy/UploadPartCopy/Completeの解析済みSDK結果を使う。同じアルゴリズム・FULL_OBJECTで比較可能なら元とコピー先のchecksum・サイズを比較する。COMPOSITEの値を、パート構成が違うコピー間で単純比較しない |

CRC32は偶発的な転送破損の検査であり、改ざんへの暗号学的保証にしない。GET側はCRC32・CRC32C・CRC64NVME・SHA1・SHA256のFULL_OBJECT値をストリーミングで照合できる構成を採る。COMPOSITEしかなく、採用SDKが本文の検証を実施できない場合は独自に未検証を検出し、検証不能のwarningを出す。外部オブジェクトの未対応アルゴリズムやchecksumなしも合意どおりwarningで継続する。不一致を検証不能として握り潰さない。SDKの設定を環境変数に上書きされて検証が省略される場合も同じ扱いとする。
FULL_OBJECTのCRC系とCOMPOSITEは明示的に区別し、ETagを本文のハッシュにしない。通常操作後に全文GETを追加せず、copy結果のsize等を得る必要がある場合だけ対応する宛先HEADを1回行う。[S3 checksum](https://docs.aws.amazon.com/AmazonS3/latest/userguide/checking-object-integrity-upload.html)、[SDKの検証設定](https://docs.aws.amazon.com/sdkref/latest/guide/feature-dataintegrity.html)

元のmtime・creation time・UNIX modeは取得できた値をS3 user metadataの`private-archive-mtime`・`private-archive-created`・`private-archive-mode`へ保存する。日時はUTC RFC3339、modeは8進数文字列とし、schema値`private-archive-version=1`で形式を識別する。ownerを含めない。local復元は一時ファイル上でbest effortに行い、失敗が本文の成功を変えないようにする。S3→S3は取得できた元属性を引き継ぎ、S3側Last-Modifiedで元mtimeを補わない。LISTだけで任意メタデータは再取得できないため、通常queryの追加HEADは行わない。

## キャンセル・timeout・後始末

- S3/STSを含むSDKクライアントはRetryConfigのmax_attempts=1をアプリから明示する。環境・共有設定で再試行回数を増やさせない。アプリによる自動リトライも行わない。[Rust SDK retries](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/retries.html)
- 接続timeoutは10秒、本文の無進捗timeoutは120秒。巨大ファイル全体に短い固定timeoutを置かない。パート・Copy・Complete等の1 API操作は15分を上限とする。正当な低速回線の転送と無進捗を区別し、値の妥当性はP1-14/15で確認する。
- Ctrl+C・最初の要求操作失敗では新規ファイル/パートを開始せず、発行済み要求を最大30秒待つ。成功応答を確認できたものは記録する。待てなかった要求はローカルtaskを停止・joinし、書込要求の成否はUnknownとして通知する。
- 続けて今回のupload IDに対するAbortMultipartUploadを1回だけ試み、最大15秒待つ。インデックス反映・一時ファイル削除にも合計5秒までを使い、最初の停止要求から50秒を後始末全体の上限にする。新規作成要求の応答を失いupload IDが分からない場合もwarningにし、全バケットのuploadを列挙して一括削除しない。
- SDKの呼出しを停止してもサーバー側で止まったとは保証しない。応答喪失を失敗確定・既存宛先不変と断定せず、Unknownとして終了する。Abort失敗・成否不明の残存分は開始1日後のLifecycle回収対象。Abortの再試行・全体rollback・独自の操作状態永続管理は追加しない。
- Copy/CompleteのHTTP 200だけで成功判定せず、SDKが本文を解析した成功応答を使う。キャンセル原因なら終了130、ほかの要求操作失敗・成否不明なら1。追加の強制終了では後始末を保証しない。

根拠: [CompleteMultipartUpload](https://docs.aws.amazon.com/AmazonS3/latest/API/API_CompleteMultipartUpload.html)、[AbortMultipartUpload](https://docs.aws.amazon.com/AmazonS3/latest/API/API_AbortMultipartUpload.html)。

## 認証とインフラの準備

アプリはAWS SDKの標準認証チェーンを使い、具体的なprofile/roleは固定しない。外部ラッパーからAWS_PROFILEを渡し、子プロセスに既存のAWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY/AWS_SESSION_TOKEN等を継承しない。SDKに共有profileのAssumeRoleを任せ、AWS CLIのセッション取得をアプリ実行時の前提にしない。credential_process等の外部依存を必要とするprofileは今回の配布・受入構成には使わない。[認証チェーン](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/credproviders.html)

P1-12でus-east-1のcress-archived-data、versioningなし、最小権限inline roleとAssumeRole専用user、multipart開始1日後のLifecycleを定義する。P1-13でユーザがWebコンソールでキーを発行し、外部の共有設定へ登録する。秘密値を文書・Terraform state・コードへ記録しない。バケット名の利用可否は導入時に確認する。
試験に必要な呼出しは対象prefixのLIST、元/宛先HEAD・GET、PUT・COPY・multipart作成/part/complete/abort、試験後DELETE。Terraformの正確なIAM actionへの対応はP1-12で確定する。バケット自動発見権限や、他プロセスのmultipart全列挙・一括中断権限は追加しない。

## Docker S3互換サーバーと障害注入

SeaweedFS OSS 4.53の単一コンテナを採用する。Apache 2.0で公開され、COPY・multipart upload/copyを持つ。version/digest、loopback公開、固定ダミー認証、専用volumeで再現可能にする。[Docker環境・起動手順・probe結果](../../tests/s3/README.md)、[公式API対応表](https://github.com/seaweedfs/seaweedfs/wiki/Amazon-S3-API)

2026-11-15にDocker起動と、PUT/GET/HEAD/metadata/CRC32、CopyObject、LISTページング、16MiBのmultipart uploadとFULL_OBJECT CRC32、UploadPartCopy、Abort、BadDigest拒否、HEAD 404をboto3のprobeで確認した。probe本文・今回のuploadは後始末済み。これは環境の互換性確認であり、製品CLI・Rust SDK adapterの検証とは区別する。

P1-14ではこのサーバーと制御可能なHTTP障害注入・fake portを組み合わせる。Dockerを製品の実行時依存にしない。

- 全対象解決前の本文APIが0回、後半のLIST/HEAD失敗でも転送未開始。
- 3方向の小容量・ゼロ長・multipart境界、末尾スラッシュと配置、深さ、両側ignore、単一キーの基準、特殊ファイル。
- 管理インデックス未作成・破損・書込不能でも本文操作成功。manifest失敗は本文開始前の要求操作エラー。
- 403と404の区別、5xxに対するSDK送信が1回、HTTP 200中のcopy/completeエラー、本文短縮・checksum不一致。
- 最初の失敗後に新規ファイルを開始しない。キャンセルの50秒上限、対象upload IDだけのAbort、Abort失敗・成否不明の通知。
- 既存local宛先の保護、属性設定失敗でも本文成功、成功分の出力・index反映と終了コード。

通常のS3互換サーバーが任意のタイミングで5xx・不正checksum・応答喪失を発生させるとは仮定しない。その部分はHTTP応答を制御する試験adapterで作る。SeaweedFS固有のAPI制限を隠すため、製品の検証を無条件に無効化することはしない。対応するchecksumと非対応のchecksumを分け、検証不能のwarningも確認する。

## 縮小した実転送と100GB相当の検証

P1-15はDockerのS3互換endpointに対する製品CLIの試験とし、実AWS認証・Terraform適用・AWSラッパーを先行条件にしない。CLI検証用のtest-fixtures featureでendpoint・path-styleをinfraへ注入し、namespaceをendpoint別に分ける。専用config/indexとダミー認証を使い、通常のprofile・indexを読まない。

1. 256MiB・1GiB・4GiBの各サイズについて、専用一時ディレクトリに非圧縮・非sparseのデータを生成し、SHA256を逐次計算する。本文生成bufferは1MiB程度とする。
2. CLIでAへuploadし、所要時間・最大RSS・パート数・manifest/一時ディスクを測る。Aを新規localパスへdownloadしてサイズ・SHA256の一致を確認する。続けてCLIでA→Bをサーバー内copyし、BもdownloadしてSHA256を確認する。
3. 3方向で実際の転送量・RSS・要求数を記録する。512MiB/差64MiBの目標を判定し、server側のRSSと区別する。最終macOS試験はwait4のru_maxrssを使い、アプリの実行時依存にはしない。
4. 1サイズずつ完了・後始末し、次のサイズを開始する。upload後にlocal入力を削除し、取得後もhash/属性の照合後に削除する。serverの遅延回収を含む保守的なpeakとして3×対象サイズとmetadata等を見込む。サーバーが圧縮・dedupしても、その節約を空き容量の前提にしない。
5. 開始前にhostとDocker VMの実際の空きを確認し、最低8GiBの余裕を残す。Docker image/cache・削除済み領域の未回収分を含めて、実際の使用量を確認する。不足時は1GiBまでの測定と明記し、4GiB成功を主張しない。試験volumeの初期化は今回専用のものだけを対象にする。
6. 実行ごとのprefix・upload ID・local生成物だけを削除し、残存を確認する。SeaweedFSは削除後のディスク回収が即時とは限らないため、サイズ間で専用volumeを初期化するか、物理使用量を確認する。全AWSリソース・実データへのアクセスは行わない。

100GB相当は実本文や巨大ファイルを作らず、次を検証する。

- 100,000,000,000 bytesと100GiBの分割計算を本番と同じplannerへ通し、5,961/6,400 uploadパート、746/800 copyパート、連続した範囲・最終パート長・10,000上限を確認する。64bitのサイズ・offsetを途中で32bitへ縮めない。
- 本番と同じスケジューラへfakeのpart reader/senderを接続し、全パートの完了・停止・後始末・最後のCompleteまでを走らせる。fakeはサイズとchecksum等のメタデータだけを返し、100GBの本文を生成・読み捨て・送信しない。
- buffer所有数の計測で4要求+1待機+1組立てを超えないことを確認する。制御経路試験のRSSと、実SDK・HTTP・本文bufferを通る縮小実転送のRSSは別に記録する。
- 第1・中間・最終パートのエラーとcancelで、新規要求の停止、部分完了情報、対象IDのAbort、成否不明を確認する。

100GBの端から端までの転送時間・実AWSの挙動・長時間接続・そのサイズでのSDK全体のRSSは未確認として残す。縮小試験とfakeによる上限検証を、100GBの実転送成功と表記しない。

## 実AWS固有の未確認範囲と費用

今回のP0/P1の検証はDocker内で完結し、AWS試験費用・100GB用ディスクは開始条件にしない。以前の約20.5 USD・100GB実S3試験案は採用しない。Docker上のCPU・メモリ・ディスクは利用するが、AWSリクエスト・保管・internet転送料金を発生させない。

実AWSのIAM・AssumeRole・Deep Archiveの復元・実際の課金・Lifecycleの経過動作はDocker試験の確認済み範囲に含めない。SeaweedFSのRestoreObjectは未対応であるため、復元関連はP2でfake port/HTTP応答により契約を検証する。Terraformは定義のfmt/validate等と、実環境での適用・動作確認を区別して記録する。実AWSを使う試験が必要になった場合は、別途小容量・費用・対象・保管期間を具体化する。

試験記録は日時、ソース識別、バイナリ・SDK/DB版、Docker image digest、OS/CPU、対象bytes、namespace、パート数・buffer・同時数、経過時間、CLI/server別RSS、host/Docker別ディスク、hash比較、要求回数、成功/失敗/Unknown、後始末を含む。認証情報は残さない。

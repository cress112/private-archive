# Phase 2・3の実装設計

2026-11-22。利用者判断D-01〜12を反映したP2-02/P3-02の詳細設計。利用者契約は[全体要件](REQUIREMENTS.md)、依存方向は[全体設計](DESIGN.md)、実装順と完了条件は[タスク](TASKS.md)を正本とする。本書は実装方式の決定であり、実装・実動作検証済みという意味ではない。

## 境界と組み立て

| 所有者 | 追加・拡張する契約 |
| --- | --- |
| domain | 5段階のLogLevelと診断の共通情報。SQL・HTTP・Slint型を含めない |
| config | logging.levelを含む有効設定/出所、保存用の型付き変更、init/set/unsetの共有サービス |
| index | SortTerm/SortSpec、一覧の対象投影（object/children/roots）、ページ/一時一覧セッション、整合ファイルの検査/取込/生成port |
| sync | ReconcileRequest/RestoreRequest/RemoveRequestと各ユースケース、保存先のファイル取得/保存port、操作結果/除外/診断/進捗のsink |
| infra | SQLiteの全実装、独立した一時manifest、local/S3のファイル保存先adapter、データ削除/復元adapter |
| CLI/GUI | 設定からの組み立て、入力、表示、明示操作と取消。互いのプロセスを呼び出す構成にしない |

ユースケースはsyncに置き、applicationクレートは新設しない。CLI/GUIは同じinfraのfactoryとconfigの解決結果を使う。SDK・DB接続型・ETagはinfraから公開せず、indexの契約に保存先の分岐を作らない。

## ファイル保存先とS3キャッシュ

- 保存先portは `open(cancel) -> FileSession`、sessionは `load() -> 整合性検査前のファイルまたは未作成`、`save(snapshot, cancel)`、`close()` を提供するobject-safeな非同期契約とする。取得条件・保存条件はsession内のadapter実装が持ち、呼び出し側にETagを渡さない。close/dropは要求操作の再実行をしない。
- ローカル保存は設定されたDBをDB adapterが直接開く。S3保存は保存先adapterが取得した通常DBファイルを同じDB adapterへ渡し、ローカル作業DBでquery/updateする。どちらもDBファイルの形式は同じで、専用envelopeや必須sidecarは不要。
- S3保存時のローカル配置は `~/.private-archive/cache/index/<識別子>/` とする。識別子はnamespaceと正規に解釈した保存先URIのUTF-8バイト列からSHA-256で作る。S3キーのdecodeやパス正規化はしない。fixtureのnamespaceを実AWSと分離する。
- ディレクトリ内の `working.db` は共有参照用の作業DB。別に、取得/保存したまま変更しないsnapshotと、そのsnapshotに対応するETagを保存先adapterが保持する。ETagはDB本体へ保存しない。
- 条件付きGETは対応を保証できるsnapshotがあるときだけ使用する。キャッシュ情報はsnapshot名とETagを一体としてatomicに公開し、並行更新で別の組合せを読まない。無効な対応情報は転送最適化に使わず、許可された取得経路で通常GETする。破損DBは正常な空DBに読み替えない。
- 保存はその実行で取得した版への条件付き保存、未作成と確認した場合は条件付き新規作成。取得失敗等で条件不明なら保存を試行せず、無条件上書きをしない。保存成功時だけ送信したsnapshotと返された版の対応を公開する。競合・失敗時の自動マージ/再試行なし。
- 作業DBの変更や前回の保存失敗を、次回の取得省略の根拠にしない。dirty/unsent等の永続フラグ、操作ジャーナル、回復キューを設けない。転送用キャッシュを失っても、通常DBファイル単体の利用・手動移動を妨げない。
- 新規キャッシュ・一時ファイルは所有者だけが読める権限にする。DB全体をVecへ展開せず、既存のストリーム転送・停止予算を使う。S3上のインデックスはStandardで保存し、データ用のcopy.data-classを適用しない。

## 共有作業DBへの取込

IndexSnapshotのportに取込sessionを追加する。開始時にDB adapter内部でローカルrevisionを捕捉し、取得ファイルを検査してからページ単位で反映する。SQLiteでは、接続内のFILE-backed TEMP表で取得済みキーを扱う。

反映対象は開始時revision以下の行と未登録行に限定し、取込開始後に別プロセスが更新した行を上書きしない。受信snapshotの最終照会日時・不明値をそのまま扱い、受信時刻をqueried_atへ書かない。ローカルrevisionはローカルDB内で増やし、受信DBのrevisionへ巻き戻さない。

全ファイルの取込が成功した場合だけ、受信側にない開始時revision以下の行をページ単位で除去できる。途中失敗・取消では取得済み反映を残し、未取込範囲を消さない。前回の未送信変更を保護するための機構にはしない。開いたworking.dbをrenameで交換せず、query/updateと同じ短いtransactionを使う。送信snapshotは既存のOnline Backup portで生成する。

取込のためのDB内部排他だけを用い、コマンド全体をロックしない。複数プロセス間の結果はローカルcommit順で反映し、実ストア全体の原子的snapshotを保証しない。

## 再照合・削除・復元

- 再照合はLoaderの実ストアcursorを使い、範囲/深さ/ignore適用後の観測を逐次反映する。正常な空範囲はfinishして成功、全件ignore・走査失敗・取消では削除照合へ進まない。集計は実際に観測・反映・削除・除外通知した件数だけで作る。
- rm/restoreは既存DiskPlanと同じ、管理indexとは独立した一時SQLite manifestのadapterを使う。identity一意制約による重複排除と処理順の索引を持ち、全URIの解決・検査を終えてsealするまで実操作を開始しない。実行終了で破棄し、次回へ引き継がない。
- rmはキー昇順、restoreはURIの入力順位→キー昇順。重複は最初に出現したURIへ割り当てる。restoreの全件事前検査では不明状態だけ追加照会し、不適格/判定不能が1件でもあれば要求0回。
- 削除/復元のportは1 objectの要求と確認済み結果を返す。S3 adapterはDeleteObject/RestoreObjectを使い、bulk deleteやBatch Operationsへ置き換えない。SDK max attempts=1を維持する。
- 成功済み結果は逐次通知・付属index反映を試みる。最初の失敗/取消からの共通50秒予算を使い、新しい実操作は開始しない。応答不明はUnknown、追加確認要求・ロールバック・再送なし。
- 使用中S3インデックスがcp元/宛先・rm/restore対象に含まれるかを、ignore適用後の全対象解決で検査する。含まれれば実操作0回で失敗。内部のインデックス転送は別portを使う。

## 設定の安全な保存

configに `read_document / validate_change / init / apply_changes` を置き、CLI cfgとGUIで共用する。変更は型付きキーで受け、復元期間やソート履歴等を保存項目に追加しない。進行中操作は開始時のSettingsを所有し、設定保存によって変更されない。

明示的な保存時だけ、設定ファイルと別の短いOSファイルロックを取得してread-modify-writeする。Rust標準File::try_lockを使い、競合なら待ち続けず詳細エラーを返す。ロックはOS handleの寿命で解放し、残るロック用ファイルの存在を実行中判定に使わない。通常の読み取りはロック用ファイルを作成しない。

保存直前に既存TOMLを読み直し、変更した項目だけ反映する。書式保持可能なTOML編集を使用し、既存の他項目を有効値/実行時上書きで埋め直さない。unset/resetは指定を取り除く。同じ親の一時ファイルへ書き、flush/sync後にatomic置換する。initは非上書き公開とし、既存ファイルには失敗する。破損設定の無断初期化や、設定変更に伴うDB移行/S3アクセスはしない。

## CLI解析・表示・診断

- 既存clapを維持する。restoreは専用のtoken列解析で、最初のURIより前の全体オプション、各URIの後の再帰/深さを明確に分離する。URIが変われば個別指定をリセットする。通常コマンドの構文・helpはCLI設計を正本とする。
- SortSpecはindexの6値enum列。空条件・未知値・同じ属性の重複指定は入力エラー。SQLへ任意文字列を渡さない。名前/URIの比較は大小文字区別・言語設定非依存。サイズ不明を0へ変換しない。
- 表示は共通メタデータから10項目の表示行へ変換する。JSONはserde_jsonのストリームwriterを使い、配列を逐次出力する。配列内のエラーはstderrへ通知し、可能なら終端を閉じて非ゼロ終了。cfg等へJSON出力を広げない。
- ログのためのグローバルな同期loggerは導入せず、型付きDiagnosticと非同期sinkで接続する。LogLevelでerror/warn/info/debug/traceを判定する。個別ignoreはdebug、直接指定ignoreはwarn、除外が起きたことと確認済み件数はinfo。traceは既存観測の段階/ページ等を出し、追加APIを発行しない。
- CLIは既存の有界・取消可能な出力経路を拡張し、結果stdoutと診断stderrを所有する。ログレベルは診断の表示を制御し、結果・成否・終了コードは変えない。GUIも操作結果/失敗/進捗のモデルを診断ログと分け、ログ抑制で操作の失敗を隠さない。

## 階層表示とソートのページ取得

indexのquery portで完全一致、再帰object一覧、直下children、rootsを区別する。childrenは仮想ディレクトリとobjectを区別し、identityはURIとkindの組とする。objectと同名prefixを混同しない。rootsはオブジェクト記録から導出する。

SQLite adapter内でSortSpecに対応した順序を構成する。任意順の全件ソートが必要な場合はFILE-backedの一時領域を使い、ページ単位だけをソートして全体順として返さない。CLIは結果全体、GUIは各列の直下を対象とする。既定URI順のkeyset queryは維持する。

GUI向けの一覧sessionはDB adapterの一時結果を所有し、ページ256件/512 KiB以下で取得する。ランダムなviewport取得が必要な場合は一時結果へ連番を付けて範囲取得し、元objectsへの大きなOFFSETを使わない。元DBのread transactionを画面の寿命まで保持しない。一時結果はその列を開く/再読込/ソートする明示操作で作り、表示を変えるまで使用する。アプリ全体のDB snapshotを起動時に固定しない。

## GUIのモデル・スレッド・配布

- Slintのmain threadにウィンドウとカスタムModelを置き、Tokio runtime/DB workerで問い合わせ・再照合・設定保存を実行する。UI更新はinvoke_from_event_loopへ渡す。キューは有界、進捗は最新値へ集約する。
- 各列は選択URI、一覧session、viewportのページcache、Loading/Ready/Failedを持つ。行データは全列合計32 MiBを上限とし、画面外ページをLRUで解放する。詳細は選択時のlocal queryだけで取得する。古い非同期応答は画面世代番号で棄却し、操作や照会自体を次回起動へ持ち越さない。
- 新しい列、再読込、ソート変更だけがローカルqueryの契機。明示再照合だけがS3の契機。再読込は設定も読み直してURIで階層を保ち、保存先変更ならルートへ戻す。ソート条件は全列共通で起動中だけ保持する。
- 再照合の操作contextは1つ。終了/取消要求後は新規操作を開始せず、workerと付属処理を共通50秒予算内で収束させる。閉じる操作に確認画面を追加しない。
- 描画はD-08のWinit＋software。P3-03でSlint/slint-build 1.19.0を採用し、現在Macの起動/描画を確認した。std・backend-winit-x11・renderer-software・compat-1-18を選び、Qt/Skia/不要な既定featureを含めない。採用版は対象OSでビルド/起動確認した版をlockfileに固定する。互換性修正は同じ操作/描画方式の範囲で行う。
- Linux ARM64はglibc環境のDebian系Dockerイメージでnative ARM64バイナリをビルドし、SeaweedFSのloopbackネットワーク名前空間に接続する。GUI検証用にX11/Xvfb・必要なライブラリ/フォントをイメージへ含める。muslや実ARM性能の保証を追加しない。
- macOSの `.app` は同じビルド成果物から生成する。小さいnative launcherをCFBundleExecutableとし、バンドル内GUI本体を相対配置で起動する。launcherは既存外部ラッパーのプロファイル選択/認証環境整理を引き継ぎ、GUI本体へプロファイル名を固定しない。argvはそのまま渡し、shell文字列評価を使わない。
- GUI本体→外側bundleの順にローカルad-hoc署名する。現在MacでFinder相当の通常起動を検証し、追加のアカウント/配布サービスを要求しない。ARM macOSは調査まで、universal binaryの実ビルド/実機動作を今回の必須範囲にしない。

## 実装時に満たす検証

CLIは既定/複数ソートを含む1万/10万/100万件でRSS512 MiB以下、10万→100万件の増分64 MiB以下。GUIも最大100万件の列を使い、行cache32 MiB以下、全体RSS512 MiB以下を設計予算とする。DB/remote応答を意図的に遅延させてもイベントループで待たず、選択/取消のイベント受付を継続できることを確認する。

主要な境界試験は、通常参照S3 API 0回、DBファイルのlocal/S3往復、キャッシュとETagの組合せ不整合回避、条件付き保存競合、取込中の別プロセスupsert保持、正常空/全件ignore/部分失敗の区別、restore全件検査前の要求0回、使用中index保護、ログ抑制時の結果/終了コードを対象とする。1回の実装変更に必要な検証を行い、P1の大容量転送試験を理由なく全件繰り返さない。

利用者判断はD-01〜12で解消済み。ライブラリpatch版・内部型名・UI余白等は上記契約内で実装時に選べる。費用/完成範囲/利用者向け契約を変える必要が出た場合だけ相談する。バケット名が利用不能の場合は、D-11に従って停止しユーザと再考する。

## 一次資料

- [Rust Fileの非ブロッキングlock](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock)
- [SQLite Online Backup](https://sqlite.org/backup.html)、[WAL](https://sqlite.org/wal.html)
- [Slint Cargo features](https://docs.rs/slint/latest/slint/docs/cargo_features/index.html)、[Winit backend](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_winit/)
- [S3条件付き保存](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html)

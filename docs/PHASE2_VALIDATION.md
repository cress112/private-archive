# Phase 2の実装・検証記録

要件は[全体要件](archived/REQUIREMENTS.md)、実装方式は[詳細設計](archived/PHASE2_3_DESIGN.md)、作業状態は[TASKS](archived/TASKS.md)を正本とする。Phase 1の結果は[従来の検証記録](PHASE1_VALIDATION.md)に分ける。実AWSは最終Phase 3まで使用しない。

2026-11-22現在、P2-03〜13を実装・検証済みで、Phase 2は完了した。次はPhase 3のGUI実装。以下は各段階で確認した内容の記録で、後続タスクへ残した事項は後の節で更新する。

## P2-03: 共有設定とcfg（2026-11-22、完了）

configクレートへ型付きConfigKey/ConfigValue/Change、検証・明示作成・短いOSファイルロック・一時ファイルからのatomic保存を追加した。既存キーは値だけを変更してコメントを保ち、symlink経由の設定ではリンクを残して参照先を更新する。実行時上書きは保存内容へ持ち込まない。

logging.levelは既定info、error/warn/info/debug/traceの5値。既存のすべての入力層を検証する。設定に不正値があれば、実行時指定で上書きしても黙って無視しない。CLIにcfgと--log-levelを接続し、設定操作の経路ではSDK・インデックスを初期化しない。

| 実行 | 現在の結果 |
| --- | --- |
| `CARGO_TARGET_DIR=target/p2 cargo test -p archive-config` | 10件成功（読込4・編集6） |
| `CARGO_TARGET_DIR=target/p2 cargo test --locked -p private-archive -p archive-domain -p sync` | 16件成功（CLI7・domain1・sync8） |
| `CARGO_TARGET_DIR=target/p2 cargo test --locked -p archive-config -p private-archive` | 最終ソースで17件成功（config10・CLI7）。上記との重複を除く検証は合計26件 |
| `cargo fmt --all --check` | 成功 |

設定の検証範囲は、未作成の参照でファイルを作らないこと、init非上書き・0600権限、5項目の編集/解除、コメント保持、相対パス・ignore escape・出所、実行時上書きの非保存、不正値/破損の拒否、ロック競合時の非書込、解放後の再利用、symlink保持。CLIでは同じ操作の終了コード・stdout/stderr、別プロセスのロック、設定以外の保存先非作成、接続先listenerへの接続0回を検証する。

CLIはエディタのRust解析とbuild先が競合するため、検証先をtarget/p2へ分けた。従来の配布済みtarget/bin/private-archiveはPhase 1の成果物であり、Phase 2検証中のバイナリと混同しない。

この段階ではcpの全件ignore時の終了コード変更、使用中インデックス保護、ignoreのinfo/debug通知、cfgを含む全CLIの取消/出力待機/fd復元の統一はP2-09に残る。cfgの実装だけをPhase 2全体の完成と扱わない。

最終実行では、保存先がルートディレクトリの場合もpanicせずエラーを返すことを含めて確認した。AWS-LCのassemblyに対するApple linkerのcompact unwind警告は出るが、リンクと実行検証は成功している。実AWSへの接続、Terraform操作、GUI実装は行っていない。

この段階ではPhase 2全体の目標は継続中だった。次はP2-04のローカル一覧/詳細/JSON/複数ソート。P2-05〜P2-13の実装・総合検証は未完了であり、この記録の26件をその代替にしない。

## P2-04: ローカル一覧・階層・ソート（2026-11-22、完了）

indexへSortTerm/SortSpecの6値、IndexBrowse/IndexListingのlocal-only port、object/directory/rootの型を追加した。SQL・ファイル形式・S3/ETagの知識は持ち込まない。SQLite adapterは元DBを短いkeysetページで読み、FILE-backedの一時結果で全体ソートし、最大256件/512 KiBのページを返す。一時結果は実行/表示内だけで破棄する。ディレクトリ/ルートはオブジェクト記録から導出し、集計値を捏造しない。

CLIにls・詳細・JSON・6値の複数ソートを接続した。全10キー/null/UTC小数秒/整数/8進数表記、古い復元状態の維持、通常参照S3 API 0回、0件/全件ignoreの終了1を確認した。日時の表現不能など結果変換途中のエラーでも、出力可能なら完了した要素をJSON配列として閉じる。

| 実行 | 結果 |
| --- | --- |
| `CARGO_TARGET_DIR=target/p2 cargo test --locked -p archive-infra --test browse --test sqlite` | 16件成功（新規browse7・既存SQLite9） |
| `CARGO_TARGET_DIR=target/p2 cargo test --locked -p private-archive` | 13件成功（一覧6・既存CLI7） |
| `CARGO_TARGET_DIR=target/p2 cargo test --locked -p private-archive --features test-fixtures --test list` | 7件成功（6件の再確認とfixture namespaceの追加1件） |

P2-04の範囲は重複を除き30件成功。全体ソートのページ境界・任意位置の取得、512 KiB制限、完全一致優先/同名prefix、深さとliteralな%/_/Unicode、隠し名の既定包含、ignore保持、取消時の観測不変、source DBを閉じた後の一時結果の利用、壊れたDBの非置換を確認した。

ネットワークlistenerを用いて、S3保存先の初回空コピーと保存済みコピーの一覧/詳細/JSON/再帰/複数ソートで接続0回を確認した。fixtureではlocal/S3両保存先のtest namespaceを正しく使い、非loopback endpointを処理前に拒否することも確認した。index本体の保存スキーマはv1のままで、S3用形式やETag列を追加していない。

通常ビルドとfixtureビルドは分けて保存する。現在の検証用通常バイナリはtarget/p2/debug/private-archive、新しいfixtureバイナリはtarget/p2-fixtures/private-archive。既存target/bin・target/fixturesのPhase 1成果物は置き換えていない。100万件/RSSの総合確認はP2-12、実AWSはP3-11へ残る。

通常版の再ビルドとfmtチェックも成功。通常版はfixture専用--test-endpointを拒否し、保存したfixture版は同引数を受理することを確認した。現在の次タスクはP2-05。

## P2-05: 明示的な再照合（2026-11-22、完了）

syncのReconcileEngineはLoaderとIndexWriteだけを受け、IndexReadを対象選択へ使わない。今回の実ストアのcursorから観測を取り、逐次反映後、全範囲の取得成功時だけfinishする。正常空範囲/完全一致の不存在は成功、全件ignore・通信/権限/必須index異常・取消は失敗として削除照合へ進まない。既存SQLiteの範囲・深さ・ignore・開始revisionによる保護を共用する。

`CARGO_TARGET_DIR=target/p2 cargo test --locked -p archive-infra --test reconcile`で5件成功。新規発見と更新/範囲内削除、同版の属性保持、正常空と完全一致の不存在、全件ignore/直接ignoreで照会0回、後続ページ失敗/権限/取消/不正なprovider応答、観測/finishの必須index異常での部分反映保持を確認した。本文openは0回、エラー後の次の対象処理も0回。

fixture版CLIのRustテスト15件が成功。`tests/s3/reconcile.py`で実CLIとSDKをloopback HTTPへ接続し、11項目が成功した。prefix/HEAD対象の限定、隠し名、深さ、外部追加/削除、正常空、全件ignore、後続ページ500、403、完全一致404、直接ignore時のHTTP 0回、応答待機中のCtrl+Cを確認した。通常lsのAPI 0回、エラー後の再試行なし、部分反映と未取得記録の保持も確認した。[機械可読結果](validation/p2-reconcile.json)。実AWSリクエストは0回。

この段階のCLIの明示再照合ではローカルindexを要求し、S3ファイル取得/保存は次のP2-06で接続した。この段階ではPhase 2全体は未完了だった。

## P2-06: 同一形式のローカル/S3インデックス（2026-11-22、完了）

syncへIndexFileStore/IndexFileSession、indexへSnapshotImport/IndexDatabaseを追加し、infraに共有ManagedIndexを実装した。ETag・条件付きGET/PUT・転送キャッシュはS3保存先adapterだけが持つ。SQLite adapterは取得前revisionを捕捉し、256件/512 KiB以下のページで取込・削除照合する。未知値/queried_atを受信時刻で埋めず、開始後の別接続更新を保持する。未作成の取込、変更検知、終了もDB共通portへ渡し、組立ての取得/保存処理からDB形式の知識を除いた。元のschema v1と通常DB形式を維持した。

転送キャッシュは変更しないsnapshotとETag/hashの組をatomicに公開し、読取側もprivate copyのhashを検査する。破損した対応情報は通常GETへ切り替えるが、不正なDB本体を空へ初期化しない。304は対応するimmutable snapshotだけを再利用し、working.dbや前回未送信分を取得省略の根拠にしない。保存は当該実行のIf-Match/If-None-Matchで変更時に1回、Standardの通常DBバイナリを逐次転送する。

| 実行 | 結果 |
| --- | --- |
| infraのimport/sqlite/reconcile/browse結合検証 | 24件成功（import新規3・既存21） |
| fixture版CLI Rustテスト | 15件成功 |
| tests/s3/index_files.py | 13項目成功、[結果](validation/p2-index-files.json) |
| tests/s3/reconcile.py | 既存11項目成功 |
| 通常版CLI/configのRustテスト | 24件成功（CLI14・config10） |
| clippy（全target、fixture有効、警告をエラー化）とfmt | 成功。既知のApple linker_messagesだけ除外 |

importでは600件のページ境界、同時更新保持、次回実行で前回未送信分を保護しないこと、不正な後続ページで部分反映を保持して未受信行を削除しないこと、取消/破棄/再利用を確認した。HTTPでは初回lsのAPI 0回、条件付き新規作成と上書き、dirty working DBと304の分離、hash不整合、外部ツール相当のDB単体の双方向移動、412と500の再試行なし、部分失敗/Ctrl+C後の一度だけの保存、取得失敗/保存失敗でもcp成功、破損DBの非置換、変更0件時のPUT 0回、所有者だけの権限を確認した。

実AWSリクエストは0回。単一PUTのインデックスsnapshot上限5 GiBは[導入手順](CLI_SETUP.md)に明記した。多重プロセスの総合試験・移行・100万件/RSSはP2-10〜12に残り、使用中S3インデックスのcp/rm/restore対象からの共通拒否はP2-09で接続する。Phase 2全体の完了を意味しない。

最終通常バイナリはtarget/p2/debug/private-archive、fixtureバイナリはtarget/p2-fixtures/private-archive。通常版が--test-endpointを拒否し、fixture版のcfgは同引数を受理しつつ設定/インデックスを作らないことも確認した。配布済みのPhase 1成果物は変更していない。

## P2-07: 複数URIの復元要求（2026-11-22、完了）

RestoreDays、URI単位の再帰/深さparser、RestoreEngine/RemoteRestore/ObjectPlanを追加し、CLIからS3Restorer/ManagedIndexへ接続した。期間は実行時必須の正整数d/w、設定の既定方式Bulk。全URIの実ストア観測を、管理indexから独立した所有者専用の一時DiskObjectsへ入れ、全件事前検査後にURI順/キー順で1件ずつ送る。重複は順序を最初のURIへ割り当て、観測は新しいものを保持する。既知の不適格な観測を重複で隠さず、不明だけ追加照会する。

| 実行 | 結果 |
| --- | --- |
| infraのrestore/object_plan結合検証 | 8件成功（restore6・manifest2） |
| fixture版CLI Rustテスト | 18件成功（新規parser2・CLI1を含む） |
| tests/s3/restore.py | 13項目成功、[結果](validation/p2-restore.json) |
| clippy 全targets/fixture有効、fmt | 成功（既知のApple linker_messagesだけ除外） |

600件の全体順・重複排除、後の重複観測で判明した復元中/状態不明による要求0回、128KiBレコード上限、終了時manifest破棄、全URI解決後の不適格検査、未知のみHEAD・照会失敗/依然未知で要求0回、古いindexの復元済み状態を判断に使わないこと、index不能でも受付成功、部分失敗/取消で次の要求を送らず受付済み反映/出力を残すことを確認した。

HTTPではSDKの実POST本文の日数/方式、LISTのRestoreStatus要求、URI別深さのリセット、隠し名/隠しディレクトリ、完全一致404/空prefix/全件ignore、直接ignoreのAPI 0回、ログ抑制、500応答不明の再試行なし、202の受付と200の既復元を区別すること、応答待機中のCtrl+C→202の確認済み受付保持→130を検証した。通常lsの状態確認はAPI 0回。復元完了待機・自動ポーリング・本文取得は行っていない。

[S3 RestoreObjectの公式仕様](https://docs.aws.amazon.com/AmazonS3/latest/API/API_RestoreObject.html)の202/200を確認し、SDK内のresponse interceptorでHTTP状態をadapter内部だけで扱う。200は新規受付の成功にしない。照会後の外部状態変化による既存一時コピーの期限変更が起こりうる旨は、当該エラーの詳細へ付記する。

実AWSリクエストは0回。使用中S3 index対象の共通保護、全CLIの取消/出力/fd統一、100万件/RSS、Docker ARM64と実運用フローは後続タスクに残る。

通常版CLIも17件成功し、fixture版とは分けて保存した。S3 LISTのRestoreStatus=falseは期限不明でも復元済み、空のRestoreStatusは不明として追加HEADへ進めることもHTTPで確認した。[RestoreStatusの公式仕様](https://docs.aws.amazon.com/AmazonS3/latest/API/API_RestoreStatus.html)に従う。

## P2-08: 確認付き削除（2026-11-22、完了）

RemoveEngine/RemoteRemove/RemoveObserverを追加し、独立DiskObjectsで全件解決・seal後、1回の確認またはauto-approveを経てS3Removerへ接続した。対象選択でIndexReadを使わず、キー昇順のDeleteObjectと確認済み記録削除/結果出力を行う。ignore・未走査深度・範囲外の記録を削除照合しない。

| 実行 | 結果 |
| --- | --- |
| infraのremove結合検証 | 4件成功 |
| fixture版CLI Rustテスト | 19件成功（rmのhelp/解析追加1件を含む） |
| tests/s3/remove.py | 12項目成功、[結果](validation/p2-remove.json) |
| clippy 全targets/fixture有効、fmt | 成功（既知のApple linker_messagesだけ除外） |

coreでは実ストア全件解決後の確認1回/件数/合計/キー順、ignore保持、確認拒否・後続列挙失敗・全件ignoreの削除0回、最初の削除失敗/取消で次の要求を止めること、確認済み結果だけの出力/記録削除、最後の記録とルートの消滅、index不能でもデータ削除成功を確認した。

HTTPとPTYでは、非TTYのnull/pipe/file拒否がstdin読取・S3照会・S3 index取得より前であること、errorログ設定でも確認表示、stdoutリダイレクト、yesだけの承認、no/YES/末尾空白/長い不承認行/EOFの終了1と再入力なし、確認待機中のCtrl+C=130と削除0回、stdin flagsの復元を確認した。auto-approveは親が書込側を閉じていないstdin pipeでも読まず完了し、確認を表示しない。

隠し名の包含、深さ/ignoreのremoteとcache両側保持、直接ignoreのAPI 0回、404/空prefix/全件ignore、後続LIST失敗、500/応答消失でUnknown・再試行/追加確認なし、破損indexの保持と削除成功、DELETE応答待機中の取消後に確認できた204の反映/出力保持・130・次のDELETE 0回を検証した。実AWSリクエストは0回。

使用中S3 indexのデータ操作からの共通保護・全CLIの取消/出力/ログ統一は次のP2-09。多重利用/移植/100万件/RSS/運用フローはP2-10〜13へ残り、この段階ではPhase 2全体は未完了だった。

## P2-09: 共通契約・出力・取消（2026-11-22、完了）

全コマンドと起動時エラー/helpを共通Sessionへ接続した。最初の失敗/取消の原因と停止予算を共有し、出力待機でも取消可能にした。終了時と2回目のSIGINTで標準入出力/エラーのfd flagsを復元する。JSONは出力可能なら現在の要素を完了して配列を閉じ、非ゼロで返す。パイプ/端末と、pollできない通常ファイル・`/dev/null`を区別する。

cpの全件ignore/空範囲を終了1へそろえ、再帰ignore個別debug・直接指定warn・実際の除外通知件数infoを接続した。使用中S3インデックスはignore適用後の対象/配置先の共通検査で終了2とし、cp・rm・restoreのデータ操作開始前に拒否する。内部の条件付きDB転送は別経路を保つ。全体helpと導入手順へ設定優先順位・隠し名・解除・参照の古さ・終了コード・停止規則を記載した。

| 実行 | 結果 |
| --- | --- |
| `cargo test --locked --workspace --all-features`（target/p2） | 118件成功、Docker専用3件は通常実行ではignored |
| fixture版CLI Rustテスト | help更新後も19件成功 |
| `tests/s3/cli_contract.py` | 17項目成功、[結果](validation/p2-cli-contract.json) |
| `tests/s3/guards.py` | 5項目成功、[結果](validation/p2-guards.json) |
| `tests/s3/faults.py` | 既存15項目成功、[結果](validation/p2-copy-faults.json) |
| index_files/reconcile HTTP再確認 | 13/11項目成功 |
| workspace全targets/all-features clippy・fmt | 成功。既知のApple linker_messagesのみ除外 |

5ログレベルの結果/終了コード不変、保存ログ設定と実行時上書き、cfg/設定保存/ls/help/reconcile/cp/restore/rmの満杯stdout待機中の取消とfd復元、stdout/stderr同一pipe、満杯stderrでの設定/引数エラー終了2、2回目SIGINTの強制130を確認した。確認済みのコピー/復元/削除/設定保存を取消時にも保持する。JSON取消は有効な部分配列で終了し、通常lsのS3 APIは0回。

snapshot生成futureを破棄した場合はqueued/running backupも停止する。workerを意図的に止めた決定的テストで、破棄済みqueued backupが開始せず、続く正常snapshotが利用できることを確認した。ネットワーク故障では未応答multipart+Abortの停止45.023秒、単一PUTの停止30.046秒で50秒以内だった。

実AWSリクエストは0回。多重利用・移行・100万件/RSS・運用はP2-10〜13へ残る。配布済みPhase 1バイナリは最終検証まで変更しない。

## P2-10: 複数プロセスの同時利用（2026-11-22、完了）

`tests/s3/concurrency.py`で独立CLIプロセスの6項目が成功した。[結果](validation/p2-concurrency.json)。別プロセスのGETを停止中でもローカルlsと別対象のcpが完了し、取込開始後に書かれた行が古いsnapshotの取込/削除照合から保護される。古いETagによる保存は1回だけ412で失敗する。次の実行では新しく取得した写しを使い、前回未送信の変更を再生しない。4つの同時lsは同じ記録をS3 API 0回で返した。

同一ETagを取得した2つのcpは両方のデータ保存が成功し、インデックス保存は1つだけ成功、もう1つはwarningで終わる。全操作の排他・自動マージ・再試行を追加していない。既存SQLite結合テストでも、独立2プロセスの同時初期化/参照/計100件書込を確認済み。

4プロセスの転送cache同時公開で非参照snapshotが残る問題を再現し、公開/旧snapshot除去の短い区間だけOS try-lockで保護した。コピー・DB・通信をロック区間に含めず、busy時は今回の任意cache公開だけを省略する。ロックファイルの存在を操作履歴や処理中フラグとして使わない。修正後は4同時公開×3回でsnapshot/ETag/hashの対応と不要ファイル0件、ロックを別プロセスで保持中でも再照合が待たず完了することを確認した。

修正後のindex_files HTTP 13項目とworkspace全targets/all-features clippyも成功。実AWSリクエストは0回。移行・件数/RSS・運用はP2-11〜13へ残る。

## P2-11: CLIの移植・同一形式DBの移行（2026-11-22、完了）

現在のx86_64 macOSで通常版/fixture版を実行し、Linux ARM64版をDockerエミュレーションで実行した。ARM macOSは公式資料と固定依存版の互換性調査とする。[移植性の詳細](PORTABILITY_VALIDATION.md)、[実行結果](validation/p2-portability.json)。

ARMを含む5項目で、共有設定/相対保存先、3方向cp、隠し名、一覧/ソート、再照合、閉じた通常DBの外部転送によるlocal→S3→local、外部追加/削除、rmと最後のルート消滅、破損DBを保持して新しい保存先へ明示再照合する経路を確認した。S3から戻したDBを両側で開き、記録とqueried_atが一致した。テスト用prefixは後始末済み。

MacのotoolとARMのlddで、追加のSQLite/TLS共有ライブラリへの依存がないことを確認した。製品の実行時にAWS CLIは不要。ARM実機性能・GUI/.appの実行を確認したという結果ではない。実AWSリクエストは0回。

## P2-12: 件数・異常系の総合検証（2026-11-22、完了）

| 検証 | 結果 |
| --- | --- |
| 最終workspace `cargo test --locked --workspace --all-features -- --include-ignored` | 126件成功、ignored 0件。Docker専用3件と後始末の追加回帰4件を含む |
| workspace全targets/all-features clippy、fmt | 成功。既知のApple linker_messagesのみclippyで除外 |
| 実CLIの1万/10万/100万件一覧 | 通常表示/size-asc,name-desc JSONの全6ケースで全件数・全体順序を逐次検証し成功。[結果](validation/p2-scale-listing.json) |
| 模擬ストアの再照合・rm・restore | 1万/10万/100万件の全9ケースで解決件数・順序・処理・後始末が成功。[結果](validation/p2-scale-operations.json) |
| S3インデックス取得/保存 | HTTP 13項目成功。[結果](validation/p2-index-files.json) |
| 再照合/復元/削除 | HTTP・PTYの11/13/12項目成功。[再照合](validation/p2-reconcile.json)・[復元](validation/p2-restore.json)・[削除](validation/p2-remove.json) |
| 共通CLI契約/使用中S3インデックス保護/既存cp障害 | 17/5/15項目成功。[共通契約](validation/p2-cli-contract.json)・[保護](validation/p2-guards.json)・[障害](validation/p2-copy-faults.json) |
| 複数プロセス | 6項目成功。[結果](validation/p2-concurrency.json) |

一覧の最大RSS（macOS time -l、bytes）:

| 件数 | 通常表示 | 複数ソートJSON |
| --- | ---: | ---: |
| 1万 | 27,652,096 | 27,389,952 |
| 10万 | 37,896,192 | 32,948,224 |
| 100万 | 37,449,728 | 39,038,976 |

再照合・削除・復元の最大RSS（同じくbytes）:

| 件数 | 再照合 | rm | restore |
| --- | ---: | ---: | ---: |
| 10,000 | 11,153,408 | 15,839,232 | 15,708,160 |
| 100,000 | 26,525,696 | 26,435,584 | 25,935,872 |
| 1,000,000 | 26,763,264 | 22,507,520 | 36,155,392 |

全ケースで512MiB以下、10万→100万件の増分64MiB以下を満たした。最大値は一覧JSONの39,038,976 bytes（約37.2MiB）。処理時間はdebugビルドと同時実行負荷を含み、実機の速度保証には使わない。

再照合は実SQLiteの全件反映/検索/snapshot生成・検査を通し、rm/restoreは実DiskObjectsの全件解決・事前検査・順序・全件操作・破棄を通した。rm/restoreのインデックスは件数だけのfake、実データ操作は本文なしfakeで、100万件のHTTP要求や本文は生成していない。テスト側も全件を配列へ保持しない。[再現手順](../tests/portability/README.md)。

大件数検証で見つけた2点を修正した。

- SQLite 3.54.1でprefix下限とcursor下限を併記すると、弱いprefix下限から毎ページ既読行を走査していた。有効な下限を1つだけ渡す方式へ変え、実2万件の先頭/末尾付近のqueryが両方6,400 VMステップとなる回帰テストで確認した。[診断と回帰結果](validation/p2-keyset-plan.json)。
- 100万件rmの初回は、データ操作後の一時DB破棄が3秒を超えて失敗した。rm/restoreの破棄を共通停止予算内の最大47秒へそろえ、最後の終了処理へ3秒を残した。仮想時間で4秒の破棄成功、最初の停止から44秒経過時の残3秒打切り、停止予算の非更新を確認した。待機を放棄した後のDropも無制限にjoinしない。意図的にworkerを止める回帰テストと、修正版の100万件rm/restoreで後始末完了を確認した。

修正前の途中/失敗計測は最終受入へ含めない。通常版とfixture版を分けて保存し、同じCargo出力先でbuildとCLI実行試験を並行しない。

通常参照S3 API 0回、外部変更の明示反映、取得途中失敗で未取得行を削除しないこと、破損DBの非置換、データ操作とindex異常の独立、最後の記録/ルートの消滅も上記HTTP/実行試験で確認した。実AWSリクエストは0回。

## P2-13: CLI運用と導入手順（2026-11-22、完了）

Macの小容量運用/移行は5項目成功した。[結果](validation/p2-workflow-mac.json)。ARMを含む同じ経路もP2-11で確認した。cfg init→archive cp→ローカル状態→restore受付→テスト側の外部完了→古いローカル状態→明示reconcile→取得/内容照合→rm→空ルートの1フローもMac/ARMの両方で成功した。[Mac](validation/p2-workflow-restore.json)・[ARM](validation/p2-workflow-restore-arm64.json)。POST/DELETEは各1回、自動ポーリングは0回。Deep Archiveの実サービス挙動をSeaweedFSが再現したという結果ではない。

[導入・設定・認証・操作・破損時の新しい保存先選択・端末移行手順](CLI_SETUP.md)を更新した。既存の外部ラッパーはリポジトリ版と同一で、更新済み通常版のhelpに全Phase 2サブコマンドを表示できることを確認した。

通常版をtarget/bin/private-archive、fixture版をtarget/fixtures/private-archiveへatomicに配置し、検証済みstaging成果物とのSHA-256一致を確認した。[ソース/成果物のハッシュ](validation/p2-artifacts.json)。以前の実行ファイルはtarget/previous-bin・target/previous-fixturesへ控えてある。ARMのfixture成果物はtarget/arm64/private-archive-fixtures。通常版とfixture版を取り違えないよう、通常版が検証専用引数を終了2で拒否することも確認した。

実AWSは[最終検証計画](FINAL_AWS_VALIDATION_PLAN.md)に従いP3-11だけで使う。今回Terraformの適用・AWSリクエスト・role/user変更は行っていない。正式バケットcress-archived-dataの利用可否で再考する条件、費用上限、テストデータだけを削除しrole/user/IaCを運用へ残す方針を維持する。GUIはPhase 3へ残る。

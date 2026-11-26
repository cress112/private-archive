# CLI 設計

2026-11-22、P2-02/P3-02の具体方式を[Phase 2・3実装設計](../PHASE2_3_DESIGN.md)に確定した。以下はPhase 1からの段階的検討記録を含み、残論点の現行解決は同実装設計を優先する。
全体設計: [../../../DESIGN.md](../DESIGN.md)。Phase 1の境界・cp結果は確定。2026-11-19に全CLIの終了コードを0/1/2/130へ統一し、rm確認のyes以外・EOFは再入力なしで1、削除成功URIと復元受付URI/日数/方式は逐次stdout、進捗はstderr端末時だけと確定した。2026-11-20にcp・ls・rm・restoreの全件ignore・空lsを含む対象0件は1、ログは既定info・再帰の個々のignoreはdebug、設定logging.levelと--log-levelによる上書きを確定した。進捗はstderr端末かつinfo有効時のみ。JSONは今回lsだけと確定し、具体的な表示・ログ・診断の接続を設計レビューで定める。

## 構成方針

- 引数・設定をコアの入力へ変換し、結果を人向け表示または機械向け出力へ変換する。
- ストア操作・インデックス更新はコアへ委譲する。
- 設定の読込・検証・保存は共有configクレートへ置き、CLI/GUIで同じ有効値・出所を扱う。TOMLやCLI引数をsyncへ直接渡さない。
- コアから標準入出力を直接参照しない構成とし、GUI再利用を可能にする。

## コマンド構文（2026-11-21、設計D-01〜03確定）

| 操作 | 形式 |
| --- | --- |
| 設定 | `private-archive cfg [init / get KEY / set KEY VALUE / unset KEY]` |
| コピー | `private-archive cp SRC DST [--recursive] [--recursive-depth N] [--data-class standard/archived]` |
| 参照 | `private-archive ls [URI] [--recursive] [--recursive-depth N] [--detail] [--json] [--sort KEYS]` |
| 再照合 | `private-archive reconcile URI [--recursive] [--recursive-depth N]` |
| 復元要求 | `private-archive restore --restore-duration 7d [全体オプション] URI [URIオプション] ...` |
| 削除 | `private-archive rm URI [--recursive] [--recursive-depth N] [--auto-approve]` |
| GUI起動 | `private-archive-gui [設定オプション]` |

- `ls` は参照専用。再照合は独立したreconcileで行い、結果件数を表示する。一覧が必要なら別のlsを実行する。旧計画のls --update-indexを別名として残さない。
- `restore` は最上位とし、ch restoreを別名として残さない。期間・方式等の全体オプションは最初のURIより前、再帰・深さは各URIの後という規則を維持する。
- CLIの共有設定オプションは --config / --index-location / --restore-tier / --ignore / --log-level。copy.data-classの実行時指定はcpの--data-classを使う。GUIは共有設定全5項目を上書きできるよう--data-classも含める。
- 同じ設定項目の優先順は、組み込み既定値→設定ファイル→起動引数。help/versionは設定・HOME・AWS認証の準備なしで表示できる既存契約を維持する。
- D-04〜06確定: reconcileは1 URI必須・非再帰は完全一致、配下は--recursiveを明示する。正常に確認した空範囲は反映して成功。全件ignoreで確認不能ならエラーとし、削除照合へ進まない。ignore発生と確認済み除外通知件数をinfo、個々のパスをdebugへ出す。権限・通信失敗を空と見なさない。

## 設計案

- 既定設定は `~/.private-archive/config.toml`、indexは`[index] location`、copyは`[copy] data-class`、復元方式は`[restore] tier`、ignoreはトップレベルの配列。解決済み値は共有configが所有する。
- ローカルパスと対応するリモートURIを解決し、コアへ設定値を渡す。
- 複数URIの復元では、URI列より前の全体オプションと、各URIに続く再帰・深さを分けて解析する。共通設定と対象ごとの範囲をコアへ渡し、URI間で再帰指定を引き継がない。構文は [CLI要件](REQUIREMENTS.md) に従い、解析器・型は設計段階で決める。
- 設定内の`~`と相対パスは共有configで解決する。設定内は設定ファイルの親、実行時指定はCWDを基準にする。既定indexは`~/.private-archive/index.db`。
- 設定ファイルの探索とインデックスの作業用コピー管理は別の責務とする。
- 通常の `ls` はローカルインデックス読み取りだけを呼び出し、初回でもS3インデックスの変更確認・取得を行わない。ローカル未作成なら空で初期化し、空の結果を返し、対象0件として終了コード1にする（2026-11-20追加方針）。
- インデックス破損時はコアが返す判明済みの箇所・原因を表示する。更新系操作ならwarningとし、lsの要求操作を完了できなければエラー終了する。自動再構築や修復目的のS3照会を開始しない。
- 通常の詳細表示や復元状態の表示も保存済み情報を使い、管理対象実オブジェクトを自動HEADしない。
- `reconcile` は指定URIと再帰・深さに応じた範囲を実ストアで照会し、インデックスへ反映して結果件数を出す。一覧は別のlsで表示する。列挙途中の失敗時は取得済み反映を残してエラー終了する。更新自体の失敗もエラーとする。
- 更新系コマンドは事前に実データを照会するコアの手順を使う。最初の要求操作の失敗で終了し、インデックスに関する取得・事前反映・結果反映・保存等の失敗はwarningとして表示する。要求操作自体の成否で正常終了・エラー終了を決める。
- JSON配列は開始記号、各要素、区切り、終了記号を逐次書く。要求操作の途中失敗時もstdoutが利用可能なら配列を閉じ、stderrにエラーを出して非ゼロ終了する。全結果のメモリ保持を前提とせず、終端処理の実装・終了コードの具体値は後続で決める。
- 結果のstdoutと、warning・エラー・進捗のstderrを分離する。JSON出力時にも診断・進捗をstdoutへ書かない。コアからの通知をCLIの表示層で振り分け、具体的な通知型は後続で設計する。
- ls詳細・JSONの基本10項目と不明値の扱いは [全体要件](../REQUIREMENTS.md) に従う。インデックスの結果を表示用表現へ変換し、元の日時とS3のLast-Modified、対象の最終照会日時とインデックス取得・表示時刻を区別する。表示のためのリモート照会は行わない。結果JSONは全体要件のsnake_case全10キーを省略せず、不明null・UTC RFC3339と小数秒保持・バイト整数・権限8進数文字列とする（2026-11-20確定）。復元状態はnot_required/archived/restoring/restored/不明null。時計だけで記録を書き換えず、期限・最終照会日時を併記する（2026-11-20確定）。
- 進捗の通知・確認・キャンセルはコアの契約として表現し、CLIが端末とのやり取りを担う案とする。
- Ctrl+CをCancelContextへ渡し、当該multipartの後始末を試みる。後始末全体の待機は50秒までとし、失敗はwarning。cpの終了コードは成功/警告のみ0、処理失敗1、入力不正2、Ctrl+C130。成功は1ファイルごとに元・宛先・バイト数をstdoutへ出す。追加の強制終了では後始末を保証しない。
- 削除確認は実対象の列挙・ignore適用後にURI・件数・合計サイズを表示し、コマンド全体で1回yesを受け付ける。確認用の集計や対象保持でも全件をメモリに積まない。manifestやディスクへの一時保存等の具体方式は設計段階で決める。
- rmのauto-approveがない場合は、リモート照会前に標準入力のTTY判定を行い、非端末なら読み取らずエラーにする。判定にはRust標準の `std::io::IsTerminal` を候補とし、タイムアウトによるパイプ判定は行わない。auto-approve時は標準入力を読まない。stdoutのパイプ・リダイレクトは許容する。根拠は [全体設計](../DESIGN.md) に記載する。

## Phase 1の実装状況

P1-01でclap deriveを導入した。共有`archive-config`は値と`BuiltIn / ConfigFile / Runtime`の出所を返す。設定の未知キー・不正値・型不整合は入力エラーとし、通常起動では設定/indexを作成しない。設定パスの明示指定先が未作成でも既定値で動作する。設定内の相対indexパスは設定ファイルの親、実行時指定はCWD基準。`~`・`~/`を展開し、`~user`は受け付けない。

CLIは`--config`、`--index-location`、`--ignore`、`--restore-tier`、`cp --data-class`を受け取り、共有設定を解決する。help/versionはHOMEや設定読込なしで表示できる。`cp`はCopyEngine・Local/S3 adapter・SQLite・独立DiskPlanへ接続済みで、3方向・属性・取消・ignoreを実CLIで検証した。cfgの全インターフェースはP2-03。

P1-11では出力を所有dup fdとnonblocking AsyncFdへ接続し、1行16KiB以下・停止後の付属処理予算を共有する。最初のCtrl+Cをコアへ通知し、2回目は終了130とする。停止原因が取消なら応答不明でも130、先行する要求操作失敗なら1を保持し、確認済みの全体成功は0。test-fixtures featureだけが専用config/index・dummy認証・loopback endpointを許可する。

外部認証ラッパーはP1-13でユーザ確認済みの20240810_shell-scriptsへ配置した。[導入手順](../../CLI_SETUP.md)・[Phase 1最終検証](../../PHASE1_VALIDATION.md)を参照。全CLIの終了コードと追加ログ要件は上記で確定し、ls以外のJSON出力は今回追加しない。移植性検証は現在Mac実動作・Docker Linux ARM64でのARM版基本動作・ARM macOSの互換性調査とする。

## ソートの設計境界（2026-11-22、D-09a〜c確定）

CLIはカンマ区切りの指定を共通のenum列へ解析し、GUIと同じSortSpecとしてindexのquery portへ渡す。文字列をSQLへ直接埋め込まない。DB adapter内で指定に対応する順序・ページcursorを構成する。ソートのために結果全体をCLI/GUIのVecへcollectしない。全件ソートに必要な一時ディスク利用はDB adapter内へ閉じ込め、既存のメモリ制約を維持する。

SortSpecは左から優先するenum列とし、NameAsc/NameDesc/SizeAsc/SizeDesc/UriAsc/UriDescの6値を持つ。外部表記は全体要件の6値に固定する。既定UriAsc、不明値は方向によらず最後、全指定キー同値ならURI昇順。通常/詳細/JSONの一覧結果全体へ同じ順序を適用し、rm/restoreの実行順序は変えない。空指定・未知値・同じ属性の重複指定は入力エラーとして扱い、曖昧な上書きをしない。


## P2-03の実装（2026-11-22）

configのConfigKey/ConfigValue/Changeとinit/apply_changesをcfgへ接続し、読み取りは有効値/出所、getはTOMLの単一値表記を返す。保存はblocking workerで行い、SDK/インデックスの初期化経路へ入らない。logging.levelと--log-levelの優先順位を共有し、ConsoleにレベルとTTY判定を渡す。新しいPreparationErrorで設定I/Oは1、不正入力は2へ分けた。

検証結果とP2-09へ残る共通契約は[Phase 2検証記録](../../PHASE2_VALIDATION.md)を参照する。

## P2-04の一覧実装（2026-11-22）

lsはIndexBrowseを呼び、S3の保存先でもinfraのindex_working_pathによるローカルコピーだけを開く。SDK/認証経路を開始しない。基本10項目を表示行へ変換し、JSONは要素単位で出力する。変換失敗時も可能な範囲で配列を閉じ、非ゼロ終了で部分結果を示す。構文・実装/検証状態は[導入手順](../../CLI_SETUP.md)と[Phase 2検証](../../PHASE2_VALIDATION.md)を参照する。

## P2-05/06の再照合と保存先接続（2026-11-22）

reconcileはsyncのReconcileEngineとinfraのManagedIndexを組み立てる。cpも同じManagedIndexを使い、ローカルは設定DBを直接開き、S3は保存先のfile sessionとDB import/snapshot portで準備・終了時保存する。通常lsとcfgはこの経路へ入らない。初期化・取得・保存の異常はcpでwarning、必須の再照合はerrorとする。データ操作の対象選択には今回の実ストア観測だけを使う。使用中インデックスの手動操作対象からの拒否はP2-09で接続する。

## P2-07の復元接続（2026-11-22）

restoreはclapで期間と最初のURIより前の共有オプションを読み、最初のURIから先を専用token parserへ渡す。URIごとの再帰/深さをリセットし、全体オプションの後置を入力エラーにする。期間は型付きRestoreDaysで正整数d/wを検証し、設定へ保存しない。

RestoreEngine、DiskObjects、S3Restorer、ManagedIndexを組み合わせる。Consoleは復元完了と区別したacceptedのURI/日数/方式を逐次stdoutへ出し、診断・ignore通知はstderrへ出す。CLIの通常閲覧で状態確認の追加HEADや完了待機はしない。

## P2-08の確認付き削除（2026-11-22）

rmは1 URI/再帰/深さ/auto-approveを解析し、非auto-approveかつstdin非TTYならSDKとインデックスの準備前に終了1にする。全対象解決後だけConsoleがstderrへURI/件数/合計を表示し、端末stdinをdupしたAsyncFdで1行を読む。yesだけを承認し、EOF/別の回答は再入力しない。長い入力も一定長のprefixだけを保持して行末まで読み、取消可能にする。

stdoutの削除成功行と診断/確認を分け、ログ抑制で確認を消さない。stdinの元のfd flagsもConsoleが初期化前に捕捉し、通常終了/最初の取消/2回目の強制130で戻す。実ストア操作はRemoveEngineとS3Removerへ渡し、indexの付属異常をデータ操作の成否へ混ぜない。

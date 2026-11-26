# Phase 3 検証記録

2026-11-23、P3-04〜P3-08の実装・検証を完了し、続いて追加要求のカラム幅調整・行高縮小・深い階層/多項目スクロールを実装・検証した。P3-03およびP3-08の節は、その時点の検証記録として残す。

P3-09のラッパー・署名済み.appも完了した。P3-10の100万件/RSS・ARM64 GUI・CLI共用の総合受入も完了した。利用者の追加指示によりIaCは定義・静的検証までとし、適用/実AWS受入は後の実施とする。

## P3-03時点の目標

P3-03「GUIの起動と共有設定・インデックス接続」までを実装・検証する。P3-04以降のカラム階層・詳細・ソート・再読込・再照合・設定編集、ラッパー/.app、Linux ARM64 GUI受入、最終AWS試験は後続タスクとする。

## P3-03: 完了

- 独立した `private-archive-gui` をworkspaceへ追加。CLIプロセスを実行せず、共有configとindexのport、infraの組み立てを使う。
- Slint/slint-buildを1.19.0に固定し、std・backend-winit-x11・renderer-software・compat-1-18を明示する。slint-build側もsoftware用のフォント生成を有効にする。Qt/Skia/FemtoVGを描画の必須経路にしない。
- 起動引数6種類と共有TOMLを接続。設定ファイルは暗黙に作らず、優先順位・下位設定層の検証・ignoreの解除はCLIと同じ処理を使う。
- 起動時にはローカル参照先を開くだけとし、設定がS3 URIでもリモート取得や認証を開始しない。未作成は空のDB、0件・全件ignoreは自然な空のバケット欄、破損は診断と既存ファイル保持とする。
- UIをmain thread、設定読み取りとDB問い合わせをworker/Tokioへ分離。有界キューでページ要求を渡し、256件以下の表示ページだけをUIへ渡す。ローカル問い合わせの取消はBrowseControlへ伝える。
- 通常の閉じる操作で取消・worker終了を開始し、最初の閉じる要求から50秒の上限で終了する。後続の実ストア再照合の接続・取消受入はP3-06/P3-08で扱う。

### 検証の範囲

Rustの起動契約テストと、現在のmacOS 12.7.6/x86_64の実ウィンドウで確認する。native受入は `test-fixtures` 限定の描画snapshot/表示モデル出力と通常の閉じるイベントを使い、ビルド成功だけを起動・描画成功とは扱わない。

network trap、metadata endpointのloopback指定、ダミーのcredential_processを一時HOMEとともに使い、S3と認証を含む要求0回を確認する。設定・インデックスはすべて一時ディレクトリで作り、実AWSと利用者の設定/DBへ接続しない。

起動接続のRustテスト8件、実ウィンドウの9ケース、配置済みCLIとの設定/参照の共用確認、静的チェックがすべて成功した。[再現手順](../tests/gui/README.md)、[起動・利用方法](../crates/gui/README.md)を参照。

根拠: [Slint 1.19のCargo features](https://docs.slint.dev/latest/docs/rust/slint/docs/cargo_features/)、[Winit/software](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backend_winit/)、[Window snapshot API](https://docs.slint.dev/latest/docs/rust/slint/struct.Window/#method.take_snapshot)。


### 現在Macでの起動・描画結果

macOS 12.7.6/x86_64でSlint 1.19.0のWinit/softwareを実行し、各ケースで1000×620の可視ウィンドウと描画snapshotを確認した。[機械可読結果](validation/p3-startup.json)を参照。

| ケース | 結果 |
| --- | --- |
| 設定・インデックス未作成 | 設定を作らず、空DBと0件の列を表示 |
| CLIとのDB共有 | 同じ通常DBから既知バケット2件をURI昇順で表示。隠しファイルの記録も既定で含む |
| 全件ignore | エラー画面にせず0件表示。保存済み記録を保持 |
| 起動引数でignore解除 | 他の共有設定上書きとともに適用し、設定ファイルを書き換えず2件表示 |
| S3保存先・初回 | リモート取得せずローカルの空の写しを表示 |
| S3保存先・既存の写し | 整合済み通常DBをバイナリコピーしただけで同じ2件を表示 |
| ローカルDB破損 | 読込失敗を画面へ表示し、元バイトを保持。閉じた時の終了コード1 |
| S3参照用のローカルDB破損 | リモート再取得・置換をせず、元バイトを保持。終了コード1 |
| 下位設定層の不正 | 起動引数で上書きする場合も不正を拒否し、DBを作らず診断。終了コード2 |

全9ケースでstdoutへ診断を混ぜず、通常の閉じる要求で終了した。配置済みCLIの `cfg init`・5項目の `cfg set`・保存値/一時上書きの `cfg get`（計8操作）から生成した共有TOMLもGUIで使用し、CLIの `ls --json` と同じ2バケットを表示した。HTTP trapの受付0、ダミーcredential_processの実行0、実AWSアクセス0。設定・DB・認証用ファイルは一時HOME配下だけを使用した。

描画例: [空の初回画面](validation/p3-startup-empty.png)、[バケット2件](validation/p3-startup-populated.png)、[破損の診断](validation/p3-startup-error.png)。配置済み通常版は `target/bin/private-archive-gui`、テスト版は `target/p3-fixtures/private-archive-gui`。[成果物ハッシュ](validation/p3-artifacts.json)を記録した。Phase 2で配置したCLI/fixtureのハッシュは変わっていない。

通常GUIの `--help`、不正引数の終了2、テスト専用引数の拒否も確認し、これらの引数処理だけでは設定・DBファイルを作らない。`otool -L` でリンク先がmacOS標準ライブラリ/frameworkだけであることを確認し、追加のDB/TLS共有ライブラリ、Qt/Skiaの導入を要求しない。Linux ARM64 GUIとARM macOS/.appの最終確認はP3-09/P3-10に残す。


### 完了時の確認

- `CARGO_TARGET_DIR=target/p2 cargo test --locked -p private-archive-gui --features test-fixtures -j 2`: 8成功・失敗0・ignore 0。
- `cargo fmt --all --check`: 成功。
- `CARGO_TARGET_DIR=target/p2 cargo clippy --locked -p private-archive-gui --all-targets --all-features -j 2 -- -D warnings -A linker_messages`: 成功。
- 通常ビルドとテスト版を分けて配置し、通常版がテスト専用引数を拒否すること、配置済みファイルと記録したSHA-256が一致することを確認した。
- 正常/空/破損のsnapshotを目視し、日本語、バケット名、保存先、エラー表示を確認した。

Apple linkerのAWS-LC由来compact-unwind警告はPhase 2と同じ既知の2シンボルであり、Clippyはこのlinker_messagesだけを対象外にしている。アプリのRust/Clippy警告を一括で無効化していない。

この段階ではバケットの初期表示までを提供する。階層選択・詳細・6値の複数ソートはP3-04、再読込はP3-05、再照合と操作中の取消/応答性はP3-06/P3-08、設定UIはP3-07、.appはP3-09、ARM GUIを含む総合受入はP3-10に残す。CLIやGUIから実AWSを操作した回数は0、IaC適用も0。


## P3-04〜P3-08: 完了

今回の目標はP3-08完了まで。カラム階層・詳細10項目・複数ソート、ローカル再読込、選択範囲の再照合、共有設定編集、診断・進捗・取消・終了の接続を実装した。

### 接続した契約

- カラムは `IndexBrowse` / `IndexListing` の使い捨て問い合わせ結果を所有し、256件/512KiB以下のページをSlintのcustom Modelへ渡す。UIは全件を配列にせず、全列共通のLRU行cacheを32MiB以下にする。SQL・DBファイル形式・S3/ETagはindexのドメイン契約へ入れない。
- URIと種別で選択を識別し、同じ名前/URIのファイルと仮想フォルダを分ける。ソートとローカル再読込で選択を復元し、消えたら残る親へ戻す。保存先変更ならルートへ戻す。画面状態は起動中だけ保持する。
- 各バケットを表示起点とし、階層を開いてもignoreの基準を変えない。indexに `View` の起点付き問い合わせと、一覧内のidentity位置取得portを追加し、具体SQLはadapterへ置いた。
- 設定の編集値は保存済みの明示値と有効値/出所を分離する。共通configの `read_document` で相対パス・`~`・ignore escape等を保存時のまま読み、変更した項目だけを共通の検証/atomic編集へ渡す。起動引数は保存しない。作成・保存・取消・項目の既定値復帰を提供し、ファイル作成と保存は明示操作だけとする。
- 通常表示・フォルダ選択・並べ替え・ローカル再読込・設定編集からS3/認証を開始しない。明示再照合だけが、同じ `ReconcileEngine` / `ManagedIndex` を呼ぶ。GUIからCLIプロセスを実行しない。
- 再照合は単一選択。ファイルは完全一致で再帰/深さを無視し、バケット/フォルダは既定無制限再帰、任意の深さは正整数だけを受け付ける。トップレベルはインデックスの既知バケットだけとし、ignoreで表示から隠れた記録も照合対象範囲として扱う。既知バケット0件ならS3照会前に失敗する。実行内で実ストアを照会し、保存済みキーを最新状態の根拠にしない。
- 再照合は同時1件。操作開始時の設定スナップショットと取消contextを所有し、画面の問い合わせと別taskで進める。進捗は実行内で確認した件数だけとし、総件数不明のまま通知できる。更新通知を集約し、診断は有界に保持する。
- 取消と閉じる操作は最初の停止時刻を共有し、50秒予算を取り直さない。取得済み反映を残し、未取得範囲の削除照合・自動再試行・実行間の未送信操作引継ぎをしない。古い問い合わせ応答は世代番号で棄却する。
- 操作部品はSlintの基本要素による共通Button/LineEdit/ComboBoxとし、software描画で表示を確認する。Slintの版・描画方式は1.19.0/Winit/softwareを維持する。

### 完了時の実測

- workspace全体: `cargo test --locked --workspace --all-features -j 2 -- --include-ignored` で141成功、失敗/ignore 0。既存のDocker試験を含む。
- 終了処理の追加後のGUI: 16成功。初回設定、同名prefix、詳細、複数ソート、URI復元/親復帰、固定ignore基準、保存先変更、設定のraw値と一括検証、row cache上限、照合対象なし/接続先不一致/事前取消の通信0、最初の取消期限の保持を確認。
- 実ウィンドウ: 起動9ケースでHTTP/認証0回。最終候補の操作15ケースで階層・再読込・設定作成/保存/取消・深さ・完全一致・全件ignore・部分失敗・取消・閉じる動作、実ポインタ/キー入力による保存、S3インデックス412競合と次回起動時の自動再送なしを確認。
- 実Docker S3: 専用prefix内の8バイト2個を使い、CLIで登録→GUI表示→別indexで外部追加→GUI再照合→CLI削除→明示ローカル再読込→最後のバケット消滅を確認。専用prefixは削除済み。既存の他データ、コンテナ、volumeを初期化しない。実AWS/IaC操作0。

機械可読結果と配置済み成果物のハッシュ:

- [起動9ケース](validation/p3-08-startup.json)
- [実画面の操作結果](validation/p3-08-workflow.json)
- [Docker S3との共用](validation/p3-08-docker.json)

.app/ラッパーはP3-09、Linux ARM64 GUI・最大100万件/RSSの総合受入はP3-10、実AWSの最終試験はP3-11に残す。


### 最終候補と導入

- GUIの最終テスト16件成功、fmt成功。workspace全体の全target/all-features Clippyを `-D warnings -A linker_messages` で確認した。除外は従来のApple/AWS-LC compact-unwind警告のみ。
- [成果物とsource SHA-256](validation/p3-08-artifacts.json): 通常版 `target/bin/private-archive-gui` と、Docker/HTTP試験版 `target/p3-08-fixtures/private-archive-gui` を分離。通常版は検証専用引数を拒否する。旧P3-03通常版は `target/previous-p3-03/private-archive-gui` へ退避し、旧ハッシュも保持した。CLIの配置済み2本はPhase 2時のハッシュを維持する。
- ネイティブの操作15ケースを最新候補で再実行した。特にボタンはポインタの押下/解放、入力欄はキー入力で開く/編集/保存/再読取まで確認し、UIコールバックだけの検証とは別に記録した。
- 最初の取消期限を、ウィンドウを閉じる処理・イベントループ後のworker待機でも取り直さないことを確認した。繰り返し取消が完了表示を「取消中」へ戻さないことも確認した。
- 実画面の通信待ち中の選択/取消と、実Docker S3の最後のバケット消滅を確認した。実AWSアクセス0、IaC適用0。Dockerは既存のS3コンテナを使い、イメージの再取得をしていない。

描画例: [カラム](validation/p3-08-columns.png)、[詳細](validation/p3-08-details.png)、[実入力後の共有設定](validation/p3-08-settings.png)、[通信中の応答性](validation/p3-08-responsive.png)、[取消](validation/p3-08-cancelled.png)、[S3インデックスの競合](validation/p3-08-index-conflict.png)、[実Docker S3の再照合](validation/p3-08-docker.png)。

P3-03の旧版と異なり、現在の通常GUIは再照合・設定編集まで提供する。初回参照S3 API 0回の契約は維持する。このP3-08時点では後続だったP3-09/P3-10も、下記の受入を経て完了した。実AWSの最終試験（P3-11）は引き続き未実施。

## 追加要求: カラム幅・コンパクトな行・スクロール（2026-11-23）

カラム右端8pxの境界を押しながらドラッグして幅を変更する。初期245px、最小160px、最大720px。行高は32pxから24pxへ縮小した。外側の表示高を固定して横スクロールを担当させ、各ListViewは列ごとの縦スクロールを担当する。列幅と同一scopeの縦位置はUIモデルから次の表示へ引き継ぎ、保存設定やindex形式には追加しない。スクロール通知から実際の縦位置を明示的にモデルへ反映し、行選択後に位置が戻ることを防いだ。

`tests/gui/columns.py` が一時ローカルDBに10階層・各階層300ファイル、合計3300オブジェクトを用意し、現在Macの実Winit/softwareウィンドウで3ケースすべて成功した。テストから表示位置を直接設定せず、ポインタ押下・移動・解放とホイール入力を送る。

| 確認 | 結果 |
| --- | --- |
| 列幅 | 245→365px、隣の列245→305pxを別々に調整。解放後の移動では変化なし。160/720pxの制限、選択・再読込後の保持、横スクロール後の境界ドラッグも成功 |
| 行高 | 24pxで描画。名前・選択表示と右側詳細を目視確認 |
| 10階層 | バケット一覧を含む12列、内容幅2940px（幅調整後2990px）、表示幅890px。ホイールで左端→途中→右端→左端を往復し、下部スクロールバーの実ドラッグでも左端へ戻れた |
| 各階層300ファイル | 2列それぞれ301項目（300ファイル＋子フォルダ）。カーソル下の列だけが移動し、縦位置は `[0,-240,0]` → `[0,-240,-480]` → `[0,-144,-480]`。逆方向・両端への移動も成功し、横位置は変わらない |
| 末尾付近のページと選択 | 右の列を-6678pxまでスクロールして実クリックで278番目の `f278.txt` を選択。詳細URI/サイズを表示し、左列-144px・右列-6678pxの両方を保持 |
| 通信と行cache | HTTP trapへの要求0、実AWS0。全snapshotで行cache32MiB以下 |

[機械可読の結果](validation/gui-columns.json)。描画: [幅調整](validation/gui-columns-resize.png)、[独立した縦位置](validation/gui-columns-independent.png)、[末尾付近の選択](validation/gui-columns-late-page.png)、[10階層の右端](validation/gui-columns-deep.png)。再現コマンドは [GUIテスト手順](../tests/gui/README.md#カラム幅深い階層列ごとのスクロール) を参照。

最終ソースでGUIのRustテスト16件、`cargo fmt --all --check`、GUI全target/featureのClippy（`-D warnings -A linker_messages`）が成功した。既存の実GUI操作15ケースも再実行してすべて成功し、設定編集・再読込・再照合の部分失敗・取消/終了・S3インデックス競合の契約を維持した。[回帰検証結果](validation/gui-columns-regression.json)を参照。CLI・設定/DB形式・IaCの変更、実AWS操作はない。

通常GUIを `target/bin/private-archive-gui` へ更新した。今回の検証用GUIは `target/gui-columns-fixtures/private-archive-gui`。旧P3-08通常GUIは `target/previous-p3-08/private-archive-gui` に退避し、[配備物とsourceのSHA256](validation/gui-columns-artifacts.json) を保存した。通常版の `--help` と不正引数/検証専用引数の拒否、引数検証でHOMEへファイルを作らないことも確認した。

この追加UI要求を受け入れた段階では全43件中40件完了だった。その後のP3-09/P3-10の成果を以下に記録する。

## P3-09: ラッパー・macOSアプリ（完了）

`scripts/private-archive-gui.sh` を実装し、既存shell-scriptsの `bash/commands/private-archive-gui.sh` にも配置した。通常GUIの6引数をそのまま渡し、`PRIVATE_ARCHIVE_GUI_BIN` / PATHで本体を選ぶ。profileは外部 `PRIVATE_ARCHIVE_AWS_PROFILE` または既定private-archive-roleを使う。直接認証・role/web/container注入・endpointの環境を整理し、共有AWS config/credentialsの指定を維持する。GUI本体のドメインへprofile名を追加しない。

`python3 scripts/package_macos.py` で `target/apps/Private Archive.app` を生成した。native Cランチャー・相対配置の通常GUI・Info.plist/PkgInfoからなる標準bundle。GUI→外側bundleの順にad-hoc署名し、`codesign --verify --deep --strict` が成功した。既存bundleを黙って上書きせず、新しい出力先へ生成する。

[wrapper試験](../tests/gui/wrapper.py)はsource/直接実行/native launcher×既定/外部profileの6ケース、空白・Unicode・shell記号を含むargvの完全保持、readonlyの親認証、PATHで同名関数へ再帰しないこと、終了コード保持を確認した。

[実.app試験](validation/p3-app.json)は2ケース成功。通常版を空白・日本語を含む別の場所へ移し、LaunchServicesの `open -n -W -a` から起動して既存設定・DBのbucketが描画されたことを実画像で確認した。fixtureを同じbundle構成から起動した場合もモデル/描画と保存設定の由来を照合。HTTP trap0回・credential_process0回・実AWS0回。生成/署名だけで実起動確認済みとはしていない。

画像: [通常版.appの実画面](validation/p3-production-app.png)、[同じbundle経由のモデル照合](validation/p3-fixture-app.png)。本作業では自分のMac用のローカル署名を使い、新しいアカウント登録・配布サービスを要求しない。他端末への署名済み配布・notarizationは今回の受入に含めない。

## P3-10: 大件数・ARM64 GUI・CLI共用（完了）

現在Macの実GUIで1列100万ファイルを扱い、3ケースすべて成功した。[実測結果](validation/p3-gui-scale.json)。1万件の予備確認も成功した。一時一覧へのINSERT文はページ内で一度準備して再利用する。256件/512KiBのページ、FILE-backedの一時結果、32MiBの行cacheという境界、DB形式・選択規則は維持している。

| GUIケース | 件数 | 最大RSS | 最大行cache | ケース全体の時間 |
| --- | ---: | ---: | ---: | ---: |
| URI昇順・末尾移動と選択 | 1,000,000 | 57.68MiB | 151,821bytes | 351.720秒 |
| size-asc/name-desc・末尾移動と選択 | 1,000,000 | 63.66MiB | 151,821bytes | 376.299秒 |
| 問い合わせ中のTop操作/取消 | 1,000,000 | 77.91MiB | 121,789bytes | 277.300秒 |

RSSは512MiB以下、行cacheは32MiB以下である。末尾へ実ホイール入力で移動し、999,977番目等の実際の行をクリックして詳細と位置保持を確認した。複数ソートの先頭256件は全100万件から有界heapで求めた期待順と一致する。通常閲覧のHTTP trapは0回。問い合わせ中のsnapshotからTop操作後のsnapshotまで1,005msで、最終的にトップ1列へ戻った。

これはunoptimized開発ビルドの測定で、1万件予備確認はRSS約63MiB・約25秒、100万件の一連の操作は約5〜6分を要する。大きな一覧の完成時間と、UIが入力を受け付ける応答性を分けて記録する。計測はMacの標準application/I/O policy、前面化、caffeinateによるsleep抑止の下でtime -lを用いた。128MiB確保の子プロセスで直接/guard付きのRSSがそれぞれ142,569,472/142,610,432bytesになることも確認し、GUIの子プロセスの計測を省略していない。画像: [100万件末尾の選択](validation/p3-million-selected.png)、[複数ソート](validation/p3-million-sort.png)、[問い合わせ中の操作](validation/p3-million-responsive.png)。

### Linux ARM64の実行

Rust 1.99.1のamd64環境からARM64へクロスコンパイルし、ELF machine=183を検査した。Debian bookworm ARM64のXvfb/Winit/software環境で実行し、`Linux aarch64` と実画面を確認した。[ARM結果](validation/p3-gui-arm64.json)。runtimeはOSのX11/フォント用ライブラリを使い、製品の追加SQLite/TLS共有ライブラリはlddに含まれない。

Mac CLIが作った通常形式DB・相対パスのTOMLをそのままARM GUIで使用し、階層・基本10項目・ソート・共有設定の保存を確認した。別indexのMac CLIで外部追加してもGUIは古いまま、選択フォルダの明示再照合で2件へ反映した。同じindexを使ったMac CLIの削除後もGUIの古い表示を保ち、明示再読込で最後のbucketが消えることを確認した。GUI保存後のlogging.level=debugをMac CLIでも参照した。

接続先は既存SeaweedFSのloopbackだけ。Mac側8333→18333の転送とARMのcontainer network namespace共有でfixture namespaceを揃えた。ホストのAWS認証は渡さず、専用prefixの7bytes×2個だけを使用・削除し、実AWS操作は0回。画像: [ARMの選択詳細](validation/p3-arm-file.png)、[ARMの再照合](validation/p3-arm-reconciled.png)。ARMはエミュレーションによる基本機能の受入であり、ARM実機性能・ARMでの100万件RSS測定ではない。ARM Macは [公式資料に基づく互換性調査](PORTABILITY_VALIDATION.md#guiのarm-macos互換性調査2026-11-23) までで、製品のARM Macビルド/実機実行は未実施。

### 最終受入の区切り

P3-09/P3-10は完了。P3-11の適用不要の要件照合・導入/移行/復旧文書・実AWS実施手順を揃えた。[受入対応表](FINAL_ACCEPTANCE.md)を参照。利用者指示に従いplan/apply・実AWS受入は実施していない。Deep Archiveの復元後取得/内容一致・空bucket確認までの実結果が揃ってから、P3-11全体を完了にする。

## 最終ソース・配備版の確認（2026-11-23）

一覧のINSERT準備をページ内で再利用する変更を含む最終ソースで、workspace全featureの142テスト（Dockerを含む、失敗/ignoredとも0）、全target/featureのClippy（`-D warnings -A linker_messages`）、fmtが成功した。AppleのAWS-LCリンク時の既知compact-unwind警告だけを許容し、Rustの警告全般は許容していない。Pythonの構文確認・wrapper受入・Terraform fmt/validateも成功した。Terraform plan/apply・実AWS要求は0回。

[GUI操作15件](validation/p3-final-workflow.json)、[列幅/10階層/300ファイルの3件](validation/p3-final-columns.json)、[Docker S3との共用](validation/p3-final-docker.json)を再実行し、すべて成功。CLI/GUIを `target/bin`、通常版.appを `target/apps/Private Archive.app` へ配備した。配備した.appも移動後のLaunchServices起動とfixtureモデル照合の[2ケース](validation/p3-final-app.json)が成功し、HTTP/認証process呼出し0回を確認した。[最終通常版の画面](validation/p3-final-production-app.png)・[fixtureの画面](validation/p3-final-fixture-app.png)。

旧CLI/GUI/.appは `target/previous-p3-09` に保持した。[最終配備物・sourceのSHA256](validation/p3-final-artifacts.json)と[整合確認結果](validation/p3-final-checks.json)を保存した。受入後にビルド専用のCargo生成物・終了済み試験の一時DB・不要なDocker image/build cacheを整理し、空き約22GiBを確認。配備物・検証記録・設定/DB・Terraform state・SeaweedFSデータ/稼働コンテナを保持した。[整理手順と実績](BUILD_CACHE.md)。全43件中42件完了で、残るP3-11は上記の実AWS受入待ち。

## 認証の責務を起動ラッパーへ修正（2026-11-23）

利用者の補足により「アプリケーションはデフォルトプロファイルを使う」とは、SDKの標準認証チェーンで環境から認証情報を解決し、アプリが名前付きプロファイルを選択しないこと、と確認した。以前のラッパーが`AWS_PROFILE=private-archive-role`を本体へ渡す実装と、SDKがAssumeRoleを行う説明を修正した。アーカイブ済みの初期要件・設計は当時の記録として更新しない。

CLI/GUIラッパーと.appのnativeランチャーは共通`private-archive-session.sh`を使い、AWS CLI v2の`configure export-credentials`で外部プロファイルから認証を解決する。必要なAssumeRoleもこの境界で行う。本体へは解決済みの認証情報・有効期限・リージョンを環境変数で渡し、`AWS_PROFILE`/`AWS_DEFAULT_PROFILE`は渡さない。SDKの標準認証チェーンを使うRust本体は変更していない。

[境界検証](validation/auth-session-wrapper.json)ではCLI/GUIの直接実行・sourceとnative launcherの10通りのプロファイル/引数を確認し、親のreadonly環境・別CWD・PATH解決・終了コード・トークン末尾の`=`・有効期限を保持した。不完全/重複/未知キーや認証解決失敗では本体を起動せず、値に含まれるshellコードを実行しない。共通スクリプトのsourceでは認証も本体起動も行わないため、既存shell-scriptsの一括読込にも対応する。

[.app検証](validation/auth-session-app.json)は通常版を移動したLaunchServices起動とfixtureモデル確認の2件が成功。実際のインストール済みAWS CLI v2.18.3を使い、一時HOMEのcredential_processからダミーの一時認証情報を取得した。ラッパー側で各起動1回、合計2回の解決を確認し、本体の閲覧による追加呼出しは0回。HTTP trap/実AWS要求は0回。実AssumeRole・実AWS権限はIaC適用後の受入に残す。

Bash/Python構文、native Cの`-Wall -Wextra -Werror`、.appのdeep/strict署名検証も成功した。更新した.appを`target/apps/Private Archive.app`、3本のshellスクリプトを既存shell-scriptsの`bash/commands`へ配置し、旧.app/外部ラッパーは`target/previous-auth-wrapper`へ保持した。[配備物のhash](validation/auth-session-artifacts.json)がこの起動経路の最新記録。従前のp3-final-artifacts.json等は変更前の受入時点の記録であり、Rust CLI/GUIのバイナリは同一のまま。Cargoの再ビルドは行っていない。

ラッパーの認証解決では起動時にSTS等へ接続する場合がある。本体の通常閲覧がS3を照会しない契約とは区別する。渡した一時認証情報は本体内で更新せず、期限切れ後は次のCLI実行/GUI再起動時にラッパーが再解決する。使い方は[CLI/GUI共通の認証準備](CLI_SETUP.md#aws-の準備実awsへの適用は未実施)を参照。

## 起動処理の正本をプロジェクトへ集約（2026-11-23）

利用者の指示により、起動・認証処理はこのプロジェクトを正本とした。CLI/GUIの`scripts/private-archive*.sh`はsourceで関数を定義する形式から実行用の入口へ変更し、プロジェクト内`target/bin/`の本体を既定とする。`private-archive-session.sh`は.appにも同梱する共通実装で、既定プロファイルは利用者設定の`private-archive`。Rust本体は変えず、認証環境変数をSDK標準チェーンで使う。

ログインシェル用の入口は、利用者の`SCRIPT_DIR`基準でこのプロジェクトの起動スクリプトへ委譲するだけにした。そこにあった認証実装のコピーは削除した。起動スクリプト・通常の使い方・プロジェクト側の試験は外部リポジトリや`SCRIPT_DIR`へ依存しない。旧ファイルは`target/previous-launcher-ownership`に保持。現在のログインシェルには旧関数定義が残る場合があるため、入口の再読込または新しいシェルで切り替える。

[プロジェクト側の境界6ケース](validation/launcher-ownership-wrapper.json)でCLI/GUI/nativeのプロファイル・引数・認証環境、親のreadonly環境、失敗時に起動しないことを確認した。別CWD・PATH未登録でもプロジェクト相対の本体を発見し、旧source操作はシェルを置き換えず明示エラーになる。[ログインシェル側の委譲4ケース](validation/launcher-ownership-adapters.json)では、SCRIPT_DIR相対の入口、source/直接呼出し、引数・終了コード・親環境の保持をダミーの委譲先で確認した。

[.appの2ケース](validation/launcher-ownership-app.json)は移動後の通常版LaunchServices起動とfixtureモデル照合が成功。実AWS CLIは一時HOMEの`private-archive`プロファイルとダミーcredential_processを使い、ラッパー側の認証解決2回・本体の追加解決0回、HTTP/実AWS要求0回。Bash/Python構文、native Cの厳格コンパイル、deep/strict署名も成功した。更新した.appを通常の配備先へ反映し、[最新の配備物hash](validation/launcher-ownership-artifacts.json)を保存した。Rust CLI/GUIバイナリは同一で、Cargoビルドは行っていない。

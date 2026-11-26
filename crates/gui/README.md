# private-archive-gui

CLIと同じ設定・通常形式のインデックスを使う、独立したSlintアプリ。カラムによる階層移動、詳細、複数ソート、ローカル再読込、選択範囲の再照合、共有設定の編集を提供する。

```sh
./target/bin/private-archive-gui
```

macOSアプリは `target/apps/Private Archive.app`。Finderから開くか、次を実行する。既存の共有設定・インデックスを使う。起動ラッパーが外部プロファイルから認証情報を解決し、本体へ環境変数で渡す。本体の通常閲覧はS3を照会しない。

```sh
open 'target/apps/Private Archive.app'
```

アプリはGUI本体・小さいnativeランチャー・共通の認証解決スクリプトを相対配置し、ローカルad-hoc署名してある。別のフォルダへ `.app` 全体を移しても利用できる。自分のMacでの利用に新しいアカウント登録は不要。再生成は `python3 scripts/package_macos.py --binary target/bin/private-archive-gui --output '/新しい保存先/Private Archive.app'`。既存アプリを上書きしないので、生成先は新しいパスにする。

起動・認証処理はプロジェクト内の [scripts/private-archive-gui.sh](../../scripts/private-archive-gui.sh) と共通sessionスクリプトを正本とする。スクリプトを直接実行すると、このプロジェクトの `target/bin/private-archive-gui` を起動する。別の本体は `PRIVATE_ARCHIVE_GUI_BIN`、認証プロファイルは `PRIVATE_ARCHIVE_AWS_PROFILE` で指定できる。既定プロファイルは `private-archive` で、`.app`も同じ処理を同梱する。

AWS CLI v2で認証を解決し、必要なAssumeRoleも起動スクリプト側で行う。本体には解決済みの認証情報とリージョンを環境変数で渡し、AWS_PROFILE/AWS_DEFAULT_PROFILEは渡さない。GUI本体はSDKの標準認証チェーンを使う。[共有設定・認証の準備とCLI/GUIの起動手順](../../docs/CLI_SETUP.md#aws-の準備実awsへの適用は未実施)を参照。認証情報の期限が切れたらGUIを再起動して再解決する。

```sh
./scripts/private-archive-gui.sh
```

設定を選んで起動する場合:

```sh
./scripts/private-archive-gui.sh --config /path/to/config.toml
```

起動引数は `--config PATH`、`--index-location PATH_OR_URI`、`--data-class standard|archived`、`--restore-tier Bulk|Standard`、`--ignore PATTERNS`、`--log-level error|warn|info|debug|trace`。優先順位は組み込み既定値→共有TOML→起動引数。起動引数は保存せず、設定保存後もその起動中は優先する。`--ignore ''` で設定済みの除外を一時解除する。

## 閲覧と明示操作

- 左から既知バケット→フォルダ→ファイルをカラムで表示する。1項目を選択すると、右隣に内容を、右の詳細欄に基本10項目を表示する。本文・サムネイルを取得せず、不明な値は不明のまま表示する。
- カラム右端の境界をマウスで押しながら左右へドラッグすると、その列の幅を変更できる（初期245px、最小160px、最大720px）。一覧の行高は24px。表示中の列幅は選択・ローカル再読込でも保持し、設定ファイルには保存しない。
- 深い階層は一覧部全体を横スクロールする。縦スクロールはカーソルが載っているカラムだけが対象で、各列の位置を独立して保持する。同じフォルダを表示している列は、選択・ローカル再読込でも縦位置を保持する。
- 「並べ替え」で名前・サイズ・URIの昇順/降順を選び、条件の追加・削除・優先順位の移動を行う。上の条件を優先し、不明値は方向によらず最後、同値ならURI昇順。条件と画面状態は起動中だけ保持する。
- 「ローカル再読込」は設定と表示をローカルから読み直す。CLI等による変更は、この操作で取り込む。選択と階層をURI/種別で復元し、消えたら残る親へ戻す。保存先が変わった場合は新しいルートへ戻る。
- GUI本体の起動・通常表示・階層選択・並べ替え・ローカル再読込・設定編集でS3へ接続しない。外部ラッパーの起動時認証解決とは区別する。インデックスがS3 URIでも参照用のローカルの写しを使い、未取得なら空表示になる。自動監視や定期取得は行わない。
- 空は空の列を表示する。新しい対象の最初の取込にはCLIの `reconcile` 等を使う。GUIはバケットを自動発見しない。

## 再照合・取消

「再照合」はS3の現在の状態を照会して、インデックスをその範囲で揃える明示操作。ファイルはその1件、フォルダ/バケットは既定で深さ無制限に再帰する。「再帰の深さ」へ正整数を入れると範囲を限定できる。ファイル選択時は深さを適用しない。「トップ」を選んだ場合は、インデックスの既知バケットだけを対象にする。

再照合は同時に1件。実行内で確認した件数と対象を表示し、事前の記録件数を実ストアの総件数とは扱わない。「取消」で新規照会を止め、取得済み反映を残す。途中失敗・取消では未取得範囲の既存記録を削除しない。正常に照会できた空範囲は成功、全件ignoreと照合範囲なしは失敗になる。

ウィンドウを閉じると確認なしで取消・後始末を行う。最初の取消/停止から最大50秒の共通予算を使い、繰り返し操作や終了待機で予算を取り直さない。自動再試行・未送信操作の持ち越しは行わない。S3インデックスの条件付き保存が競合した場合も、その実行で通知して終了する。

ignoreは共有設定と起動引数に従い、表示ではバケット起点に固定する。隠し名の暗黙除外はしない。再照合時は選択範囲を起点とする。除外発生・確認済み件数はinfo、個々のパスはdebugで診断欄へ通知する。

## 設定画面

保存されている明示値と、現在の有効値/出所を分けて表示する。相対パスや `~` は保存時の値を保つ。未作成なら「設定作成」で初期化し、既存ファイルを上書き初期化しない。「保存」は変更した項目だけを検証して保存し、「キャンセル」は未保存の編集を破棄する。「既定値へ戻す」はその項目の明示値を除く編集で、保存するまで反映しない。

保存は次の操作から適用し、進行中の再照合の設定は変えない。外部の設定変更は「ローカル再読込」で取り込む。画面を開いただけではファイルを作らず、保存先変更でDBを移動・統合・削除したり、S3から自動取得したりしない。起動引数の上書きがあれば保存後も優先する。

破損DBは画面へ診断し、既存ファイルを自動再構築しない。新しい保存先を明示的に選ぶ方法は[CLI導入・復旧手順](../../docs/CLI_SETUP.md)を参照。

## Docker S3で確認する

検証用GUIは `target/p3-final-fixtures/private-archive-gui`。通常版は検証専用の `--test-endpoint` / `--test-report` を受け付けない。既存の[Docker S3環境](../../tests/s3/README.md)を起動し、プロジェクトのルートで実行する。

```sh
demo_dir=$(mktemp -d /tmp/private-archive-gui.XXXXXX)

./target/fixtures/private-archive \
  --test-endpoint http://127.0.0.1:18333 \
  --config "$demo_dir/config.toml" \
  --index-location "$demo_dir/index.db" \
  reconcile s3://private-archive-test/ --recursive

./target/p3-final-fixtures/private-archive-gui \
  --test-endpoint http://127.0.0.1:18333 \
  --config "$demo_dir/config.toml" \
  --index-location "$demo_dir/index.db"
```

このGUIでは画面の「再照合」も同じDocker S3へ接続する。初回のバケット内が空なら空表示になる。接続先はloopback HTTPだけ、認証は固定ダミー、設定・indexの明示指定が必須。実AWS用のCLIラッパーを経由しない。検証専用URIの記録はnamespaceで分離し、異なる接続先の記録を再照合に渡した場合は認証/照会前に拒否する。

## ビルドと検証

```sh
CARGO_TARGET_DIR=target/p2 cargo run --locked -p private-archive-gui
```

Slint/slint-build 1.19.0、Winit、software renderer。UIイベントループとDB/ネットワークのworkerを分離し、列ごとの一時結果を256件以下で取得する。全列共通の行cacheは32MiB以下、診断と進捗通知も有界とする。UIの表示用状態を同期や実行再開用の状態には使わない。

[要件](../../docs/archived/gui/REQUIREMENTS.md)・[設計](../../docs/archived/PHASE2_3_DESIGN.md)・[検証記録](../../docs/PHASE3_VALIDATION.md)・[再現手順](../../tests/gui/README.md)を参照。実S3に対するGUI再照合も確認済み。[実AWS受入の現在地](../../docs/AWS_VALIDATION.md)を参照。AWS構成の参照・適用は利用者指示により対象外。

# GUI検証

## 起動検証（P3-03）

Rust側では起動引数・共有設定とローカルDB接続を検証する。ネイティブ描画の受入は、現在のMacのログイン済みウィンドウ環境で実施する。

```sh
CARGO_TARGET_DIR=target/p2 cargo test --locked -p private-archive-gui -j 2
CARGO_TARGET_DIR=target/p2 cargo build --locked -p private-archive-gui --features test-fixtures -j 2
python3 tests/gui/startup.py --binary target/p2/debug/private-archive-gui --cli target/bin/private-archive --report docs/validation/p3-startup.json --screenshots target/p3-screenshots
```

`test-fixtures`だけで提供する `--test-report` は、実際のWinit/softwareウィンドウを起動し、初期表示を取得した後にWindowのsnapshotと表示モデルを保存し、通常の閉じるイベントを送る。通常ビルドではこの引数を受け付けない。設定ファイルを明示し、インデックスはその設定または起動引数で選ぶ。接続先を注入する場合は設定・インデックス両方の明示が必須。

スクリプトは一時HOME・ダミーのcredential_process・loopbackのHTTP trapを使う。初回の空表示、配置済みCLIのcfgによる共有設定の作成・保存値/一時上書き・同形式DB共有、ignore全件除外と起動時解除、S3保存先の未取得/既存ローカル写し、破損時の元バイト保持、上書き前の設定層も検証する契約を確認する。ネットワーク要求とcredential_processの呼び出しが0であることを確認し、実AWSへ接続しない。描画snapshotのPPMをPNGへ可逆変換し、指定ディレクトリへ保存する。

P3-04以降のカラム操作、ソート、再読込、再照合、設定編集、.app、Linux ARM64 GUIの受入はこの検証には含めない。

## P3-04〜P3-08の操作・障害受入

GUIテストは起動契約に加え、カラム/同名prefix・詳細10項目・ソート/URI復元・ignore起点・設定raw値/一括検証/未作成・保存先変更・行cache32MiB・最初の取消期限を検証する。

```sh
CARGO_TARGET_DIR=target/p2 cargo test --locked -p private-archive-gui --all-features -j 2
CARGO_TARGET_DIR=target/p2 cargo build --locked -p private-archive-gui --features test-fixtures -j 2
mkdir -p target/p3-08-fixtures
cp target/p2/debug/private-archive-gui target/p3-08-fixtures/private-archive-gui
python3 tests/gui/startup.py --binary target/p3-08-fixtures/private-archive-gui --cli target/bin/private-archive --report docs/validation/p3-08-startup.json --screenshots target/p3-08-screenshots
python3 tests/gui/workflow.py --binary target/p3-08-fixtures/private-archive-gui --cli target/fixtures/private-archive --report docs/validation/p3-08-workflow.json --screenshots target/p3-08-workflow-screenshots
```

`--test-actions PATH` は `test-fixtures` 限定で、実ウィンドウに通常のUIコールバック、ポインタの押下/解放、キー入力を送る。ローカルの変更とGUIの再読込を親子プロセスで同期させ、表示を監視して自動変更する製品機能を使わない。操作完了を待ってからsnapshotを比較し、取消中の応答性は実行中のsnapshotで確認する。

`workflow.py` はカラム/ソート/詳細、外部変更前の古い表示と明示再読込、親への復帰、設定作成・保存・未保存の取消・既定値復帰・保存先変更、実ポインタ/キー入力での保存、選択フォルダ/完全一致ファイル/深さ、全件ignoreと正常空、部分失敗の保持、通信中の選択/取消/閉じる、S3インデックスの412競合と次回起動時の自動再送なしを確認する。通信先はloopback、認証はダミー、全ファイルは一時HOME/作業場所だけを使用する。

既存Docker S3を使う実接続の確認:

```sh
python3 tests/gui/docker_workflow.py --binary target/p3-08-fixtures/private-archive-gui --cli target/fixtures/private-archive --report docs/validation/p3-08-docker.json --screenshots target/p3-08-docker-screenshots
```

毎回別の専用prefixに8バイト2個だけを作り、CLIで登録→GUI表示→別indexを使ったCLIから外部追加→GUI再照合→CLI削除→GUIローカル再読込を確認する。終了時はそのprefixだけを削除し、バケットやvolumeを初期化しない。実AWSを使用しない。.app/ARM GUI/最大100万件の総合受入はP3-09/P3-10、最終AWS試験はP3-11。

## カラム幅・深い階層・列ごとのスクロール

```sh
CARGO_TARGET_DIR=target/p2 cargo build --locked -p private-archive-gui --features test-fixtures -j 2
python3 tests/gui/columns.py --binary target/p2/debug/private-archive-gui --report docs/validation/gui-columns.json --screenshots target/gui-columns-screenshots
```

10階層、各階層300ファイル（合計3300オブジェクト）の一時ローカルDBを使う。幅の拡大・縮小・上下限・解放後の移動・選択/再読込後の保持、行高24px、12列の左右往復、カーソル位置を変えた2列の独立した上下往復、256件目以降へのスクロールと実際の行クリックを確認する。ドラッグとホイールは実ウィンドウへのポインタイベントで行い、スクロール位置をテストから直接設定しない。snapshotの表示位置・モデル・画像を記録し、通信はloopback trapで0回を確認する。製品GUIへ自動操作やテスト用接続先を追加しない。

## macOSアプリとラッパー

```sh
python3 tests/gui/wrapper.py
python3 scripts/package_macos.py --binary target/bin/private-archive-gui --output 'target/apps/Private Archive.app'
python3 tests/gui/macos_app.py --app 'target/apps/Private Archive.app' --fixture target/p3-final-fixtures/private-archive-gui --report docs/validation/p3-app.json --screenshots target/p3-app-screenshots
```

生成先は新しい.appを指定する。既存bundleは上書きしない。native launcherの相対配置・引数/環境、ad-hoc署名、別の場所へ移した通常版.appのLaunchServices起動を確認する。通常版の画面はMacのwindow APIで取得し、既存bucketの描画を目視照合する。fixture版は同じ起動経路でモデルも照合する。HOME・設定・DB・credential_process・HTTP trapは一時領域に限定する。現在のランチャーはAWS CLI v2で外部プロファイルを解決するため、実AWSに接続しないcredential_processからダミーの一時認証情報を取得する。本体の閲覧では追加認証しないことを呼出し回数で確認する。

## 100万件のGUI

```sh
python3 tests/gui/scale.py --binary target/p3-final-fixtures/private-archive-gui --report docs/validation/p3-gui-scale.json --screenshots target/p3-scale-screenshots
```

一時DBに1列100万ファイルを生成し、既定URI順・size-asc/name-descの複数ソート・実ホイールで末尾付近へ移動/クリック・問い合わせ中のTop操作を実GUIで測る。期待順はheapで有界に計算し、テスト側も100万行を配列へ展開しない。RSSはMacのtime -lでGUIプロセスだけを計測し、512MiB以下・行cache32MiB以下を確認する。時間上限のbudgetはfixtureの操作ファイルだけの設定で、製品の50秒停止予算を変更しない。

## Linux ARM64 GUI

クロスbuildとruntimeは [Dockerfile.cross](Dockerfile.cross) / [Dockerfile.runtime](Dockerfile.runtime)。ARM版をXvfbで起動し、Mac CLIと同じTOML/DBを使って選択・詳細・設定保存・ソート・明示再照合・再読込を確認する。S3は既存SeaweedFSのnetwork namespace内のloopbackを使い、ホスト8333を18333へ転送して両側のfixture namespaceを一致させる。

受入後にbuild image/cacheを整理した場合は、次の手順で再生成する。sourceはコンテナへコピーし、ホストのAWS設定はmountしない。ビルド専用volumeを再利用する場合、古いsourceの生成物が残っていないことを確認する。

```sh
docker build --platform linux/amd64 -f tests/gui/Dockerfile.cross -t private-archive-gui-cross:1.99.1 tests/gui
docker build --platform linux/arm64 -f tests/gui/Dockerfile.runtime -t private-archive-gui-runtime:bookworm tests/gui
docker run --rm -d --platform linux/amd64 --name private-archive-p3-cross-builder --cpus 2 --memory 3g --cap-drop ALL --security-opt no-new-privileges -v private-archive-p2-cross-target:/target -v private-archive-p2-arm64-registry:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target private-archive-gui-cross:1.99.1 sleep infinity
docker cp Cargo.toml private-archive-p3-cross-builder:/workspace/
docker cp Cargo.lock private-archive-p3-cross-builder:/workspace/
docker cp crates private-archive-p3-cross-builder:/workspace/
docker exec private-archive-p3-cross-builder cargo build --locked -p private-archive-gui --features test-fixtures
mkdir -p target/arm64
docker cp private-archive-p3-cross-builder:/target/aarch64-unknown-linux-gnu/debug/private-archive-gui target/arm64/private-archive-gui-fixtures
docker stop private-archive-p3-cross-builder
```

```sh
python3 tests/gui/arm64.py --binary target/arm64/private-archive-gui-fixtures --cli target/fixtures/private-archive --report docs/validation/p3-gui-arm64.json --screenshots target/p3-arm-screenshots
```

ホストのAWS認証は渡さず、bind mountは一時作業ディレクトリだけ。専用prefixの小さい2個だけを作成し、試験後に削除する。ARM実機の性能とARM macOS実行の確認には代えない。

ビルド・受入の合間に [cache整理手順](../../docs/BUILD_CACHE.md) で空き容量を確認する。配備物を含むtarget全体やS3データvolumeを削除しない。

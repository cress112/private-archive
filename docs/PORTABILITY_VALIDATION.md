# 移植性の検証方針と確認結果

2026-11-20。要件の正本は [全体要件](archived/REQUIREMENTS.md)。P2-11・P3-02・P3-10で製品の検証結果を追記する。初回は検証基盤を確認した。2026-11-22の製品CLI実行結果は下記P2-11の節を参照。2026-11-23には現在Macの実.app起動とLinux ARM64 GUIも実行・描画・基本操作を確認した。

## 合意した受入範囲

| 対象 | 今回確認すること |
| --- | --- |
| 現在のx86_64 macOS | CLI・GUIのビルドと実動作 |
| Linux ARM64 | Dockerエミュレーションで製品のARM版を実行し、S3互換ストア接続と基本機能を検証。GUIは起動・描画・基本操作も確認 |
| ARM macOS | Rust・採用クレート・GUIバックエンド等の公式資料による互換性調査。ビルド・実機実行は必須にしない |

設定・同形式のインデックスを移して利用できること、追加のDB/TLS共有ライブラリ・AWS CLIを製品の実行時依存にしないことを検証する。ARM実機の性能・操作性をDockerの結果から保証しない。

## ARMコンテナとS3接続の予備確認

- ホストは `Darwin x86_64`、Dockerサーバーは `linux/amd64`。
- `docker run --platform linux/arm64` で `python:3.13-alpine` を実行した。取得したイメージのmanifest digestは `sha256:9ef1a599f863231c3d91e941389bfdc4a2d6070557243d54df1a4e57015b960e`。
- Pythonが報告した環境は `Linux / aarch64`。署名付き `ListObjectsV2` を既存SeaweedFSへ1回送り、HTTP `200`、`ListBucketResult`、返却オブジェクト0件を確認した。作成・更新・削除は0件、実AWS通信も0件。
- SeaweedFSは既存の `private-archive-s3-tests-s3-1`。コンテナのネットワーク名前空間を `--network container:private-archive-s3-tests-s3-1` で共有し、`http://127.0.0.1:8333` へ接続した。認証はcomposeの固定dummy値。fixtureのloopback限定・本番namespace拒否を緩めない構成とする。
- ARM側はread-only、capabilityなし、no-new-privileges、128MiB・1CPU、終了時自動削除の一時コンテナとした。ホストのソース・AWS認証情報・利用者データはマウントしていない。

これはエミュレーションとネットワーク経路の確認であり、製品CLIのS3 adapter・SQLite・取消・GUI等の検証結果ではない。製品側の検証は対応タスクで行う。

## 公式資料の調査

- [Docker multi-platform builds](https://docs.docker.com/build/building/multi-platform/) はDocker DesktopのQEMUによるエミュレーションを説明している。コンパイル等はネイティブより遅くなりうるため、機能試験と性能実測を分ける。
- [RustのApple Darwinターゲット](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html) は `aarch64-apple-darwin`、macOS 11.0以降、Cコードのビルド・リンクで必要になるClang/macOS SDKの条件を記載している。
- [SlintのmacOS対応](https://docs.slint.dev/latest/docs/slint/guide/platforms/desktop/macos/general/) はmacOS 15/26のaarch64を検証対象としている。採用バージョン・バックエンドを決めた後、その版のx86_64 macOS・ARM macOS対応と依存関係を改めて照合する。
- [SlintのLinux対応](https://docs.slint.dev/latest/docs/slint/guide/platforms/desktop/linux/general/) はWaylandまたはX11、glibc、D-Busの環境を挙げている。ARM GUIの検証用ウィンドウ環境・レンダラはP3-02で定める。

## macOSアプリ化の調査（2026-11-21）

- 現在MacはmacOS 12.7.6、`/usr/bin/codesign`を利用できる。
- [Appleのバンドル構成](https://developer.apple.com/documentation/bundleresources/placing-content-in-a-bundle) と [SlintのmacOS Packaging](https://docs.slint.dev/latest/docs/slint/guide/platforms/desktop/packaging/macos/) を確認。後者は、自分のMacで動かすためのad-hoc署名を記載する。
- 個人利用向け.appの自動生成、起動ラッパー相当の同梱、既存設定・AWSプロファイルの再利用をP3-09/P3-10へ含める。GUI本体へ認証プロファイルを固定しない。
- 現時点では調査・方針決定まで。製品の.app生成・署名・Finderからの起動、SlintバックエンドのmacOS 12互換性は未検証。実装後に現在Macで確認する。

## CLIのARM macOS互換性調査（2026-11-22）

CLIの依存関係にARM macOSを妨げる既知の条件は見つからなかった。以下の公式資料と固定版ソースの照合に基づく判断であり、製品のARM Macビルド/実機実行は未実施。

| 対象 | 確認した根拠 |
| --- | --- |
| Rust 1.99.1 / edition 2024 | [Rust Darwin target](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html)のaarch64-apple-darwin、macOS 11以降。CコードのビルドにはClang/macOS SDKが必要 |
| Tokio 1.54.1 | [固定版の対応プラットフォーム](https://docs.rs/tokio/1.54.1/tokio/#supported-platforms)とUnix signal/AsyncFdを利用。製品にx86固有のfd構造体や命令を持ち込んでいない |
| rusqlite 0.43.0 / libsqlite3-sys 0.40.0 | [bundled仕様](https://github.com/rusqlite/rusqlite#notes-on-building-rusqlite-and-libsqlite3-sys)と固定版build.rsを確認。SQLiteをCコンパイラで同梱ビルドし、OSのSQLiteやsqlite3コマンドを要求しない |
| AWS SDK / rustls / aws-lc-sys 0.47.0 | [AWS-LCの対応表](https://aws.github.io/aws-lc-rs/platform_support.html)と[Apple build条件](https://aws.github.io/aws-lc-rs/requirements/apple.html)、固定版builder/main.rs・sys_bindgen.rsにaarch64-apple-darwinの分岐があることを確認。本製品はnon-FIPS構成で、追加の共有TLSライブラリを選んでいない |

GUI/Slint/.appの判定はPhase 3で行う。CLIの調査結果をGUIの実行検証へ流用しない。

## 現在MacのPhase 2 CLI（2026-11-22）

x86_64 macOS 12.7.6で、通常版とfixture版をビルドした。最終workspace試験はkeyset・後始末回帰テストとDocker専用3件を含む126件すべて成功、ignored 0件。通常版のhelp・共有設定参照・空のローカル一覧はAWS認証なしで動き、fixture専用引数は終了2で拒否した。小容量の3方向コピー・DBのlocal/S3往復・外部変更・削除・破損時の明示的な新規保存先選択は[運用試験結果](validation/p2-workflow-mac.json)を参照。

`otool -L`で通常版が参照する共有ライブラリはSecurity.framework・CoreFoundation.framework・libiconv.2.dylib・libSystem.B.dylibのみだった。追加のSQLite/TLS共有ライブラリとAWS CLIを実行時依存にしていない。最終配置用通常版はtarget/p2-bin/private-archive、fixture版はtarget/p2-fixtures/private-archiveへ分け、最終検証後、target/bin・target/fixturesも更新した。以前の実行ファイルはtarget/previous-bin・target/previous-fixturesへ控えた。

## P2-11: Linux ARM64製品CLIと設定/DB移行（2026-11-22、完了）

Rust 1.99.1のamd64 Dockerからaarch64-unknown-linux-gnuへクロスコンパイルし、ELF64のmachine=183を検査した。ビルド用Dockerfile/手順は[runner](../tests/portability/README.md)を参照。完成したfixture版CLIをDockerのlinux/arm64環境で実行し、Linux aarch64を確認した。[実行結果](validation/p2-portability.json)の5項目が成功した。

- Macで作った設定/通常DBをARM側で開き、相対保存先・共有設定編集・隠し名を含む一覧を確認した。
- ARMからlocal→S3・S3→S3・S3→localの小容量cp、内容照合、再照合を実行した。接続先は既存SeaweedFSのloopbackで、ホストのAWS認証は渡していない。
- 閉じた通常DBを外部転送でS3へ置き、保存先変更後にARMで取得/再照合した。S3の通常DBを外部転送でローカルへ戻し、両側で同じ記録/queried_atを参照できた。独自のenvelopeや必須sidecarは使っていない。
- 外部追加/削除は明示再照合で反映し、ARMのrmで最後の記録とルートが消えた。破損DBを保持し、新しい保存先を明示して再照合する経路も確認した。テスト用prefixは終了時に空へ戻した。
- ARM版のlddはlibgcc_s.so.1・libm.so.6・libc.so.6・ld-linux-aarch64.so.1だけだった。追加のSQLite/TLS共有ライブラリとAWS CLIを要求しない。

さらにnetwork=noneの別ARMコンテナ内でloopback模擬HTTPを立て、同じARM版CLIのarchive→restore受付→明示reconcile→取得→rmを確認した。[結果](validation/p2-workflow-restore-arm64.json)。外部のテスト側だけで復元完了状態を変化させ、製品の自動ポーリングは0回。実AWSリクエストは0回。

本結果はエミュレーションによる基本機能の確認で、ARM実機性能やGUI/.appの実行結果ではない。ARM macOSは前節の公式資料/固定依存版調査の範囲とする。

## GUIのARM macOS互換性調査（2026-11-23）

ARM Macは実機を使用せず、公式資料とCargo.lockで固定した依存構成を照合した。Rustは [aarch64-apple-darwinを提供](https://doc.rust-lang.org/rustc/platform-support/apple-darwin.html)し、ARM64向けのOS下限はmacOS 11。Slintの[公式macOS検証対象](https://docs.slint.dev/latest/docs/slint/guide/platforms/desktop/macos/general/)はaarch64のmacOS 15/26である。採用したSlint 1.19.0 / Winit 0.30.13の固定版sourceにmacOS実装があり、選択したsoftware rendererとGUIの公開構成にx86専用の前提を追加していない。

以上から、現行構成をARM Macで再ビルドする方針と矛盾する既知の条件は見つからない、という調査上の判断とする。製品のARM Macビルド・実機動作、現在のx86_64 macOS 12での成功からARM macOS 12の動作を保証するものではない。RustのOS下限とSlintの公式検証範囲も区別する。

`.app` は [SlintのmacOS Packaging](https://docs.slint.dev/latest/docs/slint/guide/platforms/desktop/packaging/macos/) の標準bundle/ローカル署名に沿って、自分のMac用のad-hoc署名で生成した。native launcherも移行先のMac SDKでGUIと同じarchitecture向けに再生成する。今回のアプリは現在Mac向けx86_64であり、universal binaryではない。

## P3-10: Linux ARM64の製品GUI（2026-11-23、完了）

Debian bookworm/XvfbのARM64環境でSlint 1.19.0/Winit/softwareのGUIを実行し、ELF machine=183、`Linux aarch64`、可視ウィンドウの描画を確認した。[実行結果](validation/p3-gui-arm64.json)・[画面](validation/p3-arm-file.png)。コンパイル成功だけで実行済みとしていない。

Mac CLIの通常DB・同じ相対パス設定をARM GUIで開き、階層/詳細・ソート・設定保存を確認した。Mac CLIからの外部追加→ARM GUIの明示再照合、共有DBから削除→GUIの明示再読込→最後のbucket消滅も成功。Linuxで保存したlogging.levelはMac CLIで参照できた。lddにSQLite/TLSの追加共有ライブラリはなく、GUI用のOSウィンドウ/フォントライブラリをruntimeに含めた。

SeaweedFSのloopbackとdummy認証を使い、一意の試験prefixの7bytes×2個だけを清掃した。実AWS0回。Dockerエミュレーションの基本機能の受入であり、ARM実機性能は未確認。100万件/RSSのGUI予算は現在Macで実測した。ARM Macは前節の資料調査までとする。

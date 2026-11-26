# 塩漬けデータアーカイブ

個人データをAWS S3へ長期保存するRust製CLIと、Finder風のカラム表示で参照するGUI。CLIとGUIで設定・インデックスを共有します。通常の一覧表示はローカルの写しを使い、S3への照会は明示的に行います。

## 基本的な使い方

ビルド済みCLIは `target/bin/private-archive`、GUIは `target/bin/private-archive-gui`。プロジェクト内の起動スクリプトがAWS CLI v2でプロファイルを解決し、本体へ認証情報を環境変数で渡します（[認証と起動手順](docs/CLI_SETUP.md#aws-の準備実awsへの適用は未実施)）。本体はSDKの標準認証チェーンを使います。設定の既定パスは `~/.private-archive/config.toml`（`--config PATH` で変更可能）。

```sh
./target/bin/private-archive --help
./scripts/private-archive.sh cp ./file.txt s3://YOUR_BUCKET/archive/
./scripts/private-archive.sh reconcile s3://YOUR_BUCKET/archive/ --recursive
./scripts/private-archive.sh ls s3://YOUR_BUCKET/archive/ --recursive
./scripts/private-archive-gui.sh
# macOSではFinderから開くことも可能
open 'target/apps/Private Archive.app'
```

起動・認証処理の正本はこのプロジェクトの `scripts/` です。起動スクリプトはプロジェクト内の `target/bin/` を参照するため、カレントディレクトリやPATHへの本体登録に依存しません。スクリプトはsourceせず実行します。

既定のAWSプロファイルは `private-archive`。`~/.aws/config` の設定例（`ACCOUNT_ID` は対象アカウントIDへ置換）:

```ini
[profile private-archive]
role_arn = arn:aws:iam::ACCOUNT_ID:role/private-archive-role
source_profile = private-archive-user
region = us-east-1
```

`~/.aws/credentials` の `[private-archive-user]` にIAMユーザのアクセスキー・秘密キーを設定します。起動スクリプトがAssumeRoleを含めて認証を解決します。別プロファイルは `PRIVATE_ARCHIVE_AWS_PROFILE`、別ビルドの本体は `PRIVATE_ARCHIVE_BIN` / `PRIVATE_ARCHIVE_GUI_BIN` で指定できます。`.app` も同じ認証処理・既定プロファイルを使用します。

`cp` はローカル→S3／S3→ローカル／S3→S3に対応し、`restore` は復元要求、`rm` は確認付き削除、`cfg` は設定を扱います。再帰では隠しファイルも対象です。除外はignore設定か `--ignore` で指定します。

GUIの「ローカル再読込」は手元の変更を画面に反映し、「再照合」はS3の現在の状態を照会します。初回はCLIの `reconcile` 等で対象を取り込みます。詳しくは [CLI導入・操作・復旧](docs/CLI_SETUP.md)／[GUIの使い方](crates/gui/README.md)。

## 開発

Rustの必要バージョンは [Cargo.toml](Cargo.toml) の `rust-version` を参照。C/C++ビルド環境が必要です。macOSではXcode Command Line Toolsを使用します。以下はプロジェクトルートで実行します。

```sh
# 配備物とビルドcacheを分ける
export CARGO_TARGET_DIR="$PWD/target/p2"
cargo build --locked -p private-archive -p private-archive-gui -j 1
cargo run --locked -p private-archive -- --help
cargo run --locked -p private-archive-gui
cargo test --locked --workspace -j 1
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
```

workspaceはCLI／GUI、共有設定・ドメイン、sync／indexのコア、S3／SQLite等のinfraで構成します。ビルド結果は `target/p2/debug/` に生成され、`target/bin/` や既存.appは自動更新されません。.appの生成は [GUI手順](crates/gui/README.md)を参照。

DockerでのS3互換試験は [S3検証手順](tests/s3/README.md)、画面・ARM64の試験は [GUI検証手順](tests/gui/README.md)。Docker起動後、全featureの試験は `cargo test --locked --workspace --all-features -j 1 -- --include-ignored` で実行します。ディスク容量とcache整理は [ビルドcache管理](docs/BUILD_CACHE.md)を参照してください。

## 関連資料

- [CLI導入・移行・復旧](docs/CLI_SETUP.md)／[GUI操作](crates/gui/README.md)／[Terraform](terraform/environments/main/README.md)
- 検証記録: [Phase 1](docs/PHASE1_VALIDATION.md)／[Phase 2](docs/PHASE2_VALIDATION.md)／[Phase 3](docs/PHASE3_VALIDATION.md)／[移植性](docs/PORTABILITY_VALIDATION.md)
- [受入対応表](docs/FINAL_ACCEPTANCE.md)／[実AWS受入手順](docs/AWS_ACCEPTANCE_RUNBOOK.md)
- [初期要件・初期設計・タスクのアーカイブ](docs/archived/README.md)

実装とDocker等での受入はP3-10まで完了しています。100GB実本文の転送は未検証です。開発ビルドの100万件GUI検証では最大RSS約78MiB、一連の一覧操作に数分を要しました。

<!-- aws-acceptance:start -->
実AWS受入: **完了（全43件完了）**。[検証結果と費用台帳](docs/AWS_VALIDATION.md)。
<!-- aws-acceptance:end -->

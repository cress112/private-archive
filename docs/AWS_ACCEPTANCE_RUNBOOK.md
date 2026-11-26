# 適用後の最終AWS受入手順

2026-11-23の最新指示により実AWSのアプリ動作受入を開始した。AWS構成の参照・plan/apply・権限追加は禁止。以下の準備時点の記録に対し、実施中の結果・数量枠・復元待ちは [実AWS検証記録](AWS_VALIDATION.md) と [台帳](validation/aws-live.json) を優先する。
2026-11-23、実施準備のみ。利用者の指示により、この手順のAWS操作・Terraform plan/applyは未実施。[確定計画](FINAL_AWS_VALIDATION_PLAN.md)が数量・費用・保持・後始末の正本。実施日に公式料金を再確認し、追加費用1 USD以内の見積りと数量台帳を確定してから開始する。

## 適用と認証

管理者が対象account/regionと新規リソースだけのplanを確認し、保存したplanを適用する。既存リソースの自動import、bucket名の自動変更はしない。

```sh
terraform -chdir=terraform/environments/main init -input=false
terraform -chdir=terraform/environments/main fmt -check
terraform -chdir=terraform/environments/main validate
# 認証済みの管理用profileを操作者が選ぶ。
AWS_PROFILE=YOUR_OPERATOR_PROFILE terraform -chdir=terraform/environments/main plan -out=approved.tfplan
AWS_PROFILE=YOUR_OPERATOR_PROFILE terraform -chdir=terraform/environments/main apply approved.tfplan
terraform -chdir=terraform/environments/main output
```

planは `cress-archived-data`、`private-archive-role`、`private-archive-source` とその最小権限/暗号化/公開遮断/Lifecycleだけであること。bucket名を使用できなければ停止して相談する。新規bucketのversioning未設定、SSE-S3、公開遮断、開始1日後からの未完了multipart中断、object削除/移行Lifecycleがないことを管理用認証で確認する。Lifecycleによる実時間1日ちょうどの削除を試験完了条件にしない。

アプリ本体はSDKの標準認証チェーンを使う。起動ラッパーがAWS CLI v2で外部role profileを解決し、AssumeRole済み認証情報を環境変数で本体へ渡す。sourceユーザのkeyは操作者がAWSコンソールで発行し、[既存のprofile手順](CLI_SETUP.md#aws-の準備実awsへの適用は未実施)に従ってローカル共有認証へ登録する。Terraformでkeyを発行しない。roleは専用sourceだけを信頼し、sourceはそのroleへのAssumeRoleだけ、roleは対象bucketのListBucketとobject操作だけであることを確認する。profile名・公開ARNは記録できるが秘密値は保存しない。

## 数量台帳とローカル準備

専用の一意prefixを1個決め、操作名/時刻/結果/要求数/作成key/bytesを逐次記録する。全キーは台帳で追跡し、失敗した要求も予算へ数える。操作数の不明が生じたら多めに見積もり、最大1000回へ近づいたら追加試験を止めて清掃を優先する。推測の最終請求額を成功値として記録しない。

```sh
archive_test_dir=$(mktemp -d /tmp/private-archive-aws.XXXXXX)
archive_test_root="s3://cress-archived-data/acceptance-$(date -u +%Y%m%dT%H%M%SZ)/"
archive_test_cli="$PWD/scripts/private-archive.sh"
private-archive() { /bin/bash "$archive_test_cli" "$@"; }
private-archive --config "$archive_test_dir/config.toml" cfg init
private-archive --config "$archive_test_dir/config.toml" cfg set index.location "$archive_test_dir/index.db"
python3 - "$archive_test_dir/sample.bin" <<'PY'
import pathlib, sys
pathlib.Path(sys.argv[1]).write_bytes(bytes(range(256)) * 256)  # 64 KiB
PY
```

新設bucketに試験外データがないことを最初に確認する。root/CLI/configの変数値とSHA256を記録し、64KiBを超えるデータや大件数のS3要求を追加しない。本文なしの障害注入・100万件/RSS・大容量相当は既存Docker/fake試験の証拠を使う。

## 3方向コピーと明示操作

下記はすべて同じ共有configを使う（各行で `--config "$archive_test_dir/config.toml"` を付ける）。サンプル1個からStandard2個と取得ファイルを作る。

```sh
private-archive --config "$archive_test_dir/config.toml" cp "$archive_test_dir/sample.bin" "${archive_test_root}data/source.bin"
private-archive --config "$archive_test_dir/config.toml" cp "${archive_test_root}data/source.bin" "${archive_test_root}data/copy.bin"
private-archive --config "$archive_test_dir/config.toml" cp "${archive_test_root}data/copy.bin" "$archive_test_dir/download.bin"
cmp "$archive_test_dir/sample.bin" "$archive_test_dir/download.bin"
private-archive --config "$archive_test_dir/config.toml" reconcile "${archive_test_root}data/" --recursive
private-archive --config "$archive_test_dir/config.toml" ls "${archive_test_root}data/" --recursive --sort size-asc,name-desc --json
```

64KiB・SHA256・取得サイズ・可能な元属性を確認する。不明属性は不明のまま記録する。通常lsとGUIの閲覧/ローカル再読込はS3/認証要求を行わない既存契約を維持し、鮮度確認のために要求を追加しない。

## 通常DBの往復と条件付き要求

1. CLI/GUIを閉じてDBをcheckpoint済みの通常単体ファイルにし、外部AWS CLIの `s3 cp` で `${archive_test_root}index/index.db` へ置く。本文サイズ4MiB以下・Standard・SHA256を確認する。外部転送にも専用roleを使う。
2. configのindex.locationをそのS3 URIへ切り替え、明示reconcileで取得・取込・条件付き保存を行う。同じ対象の再照合ではqueried_atが当該実行の値になることを確認する。通常GUI/lsでは取得しない。
3. 外部 `s3 cp` でS3のDBを新しいローカルパスへ取得し、設定の向き先をそのパスへ変える。同じ記録・通常スキーマで閲覧でき、独自envelope/sidecarを要求しないことを確認する。
4. 実サービスの条件付き取得は `s3api get-object --if-none-match` に現在のETagを指定し、304を1回確認する。JSON/ETag/要求IDだけを記録し、認証headerは保存しない。
5. ETagを保存した後に、製品のS3 indexを使った明示reconcileを1回実行して新しい照会時刻のDBを保存する。本文の版が変わったことを確認する。その後、保存していた古いETagへの `s3api put-object --if-match` を1回だけ実行し、412と現行本文不変を確認する。これは実サービスの条件式の確認。製品が412を失敗として通知し再送しない経路は既存HTTP/GUI試験で検証済みであることを区別する。

同じDBバイトを再PUTするだけではETagが変わらない場合があるので、版変更の確認を省略しない。競合に時間依存の並列実行や大量の反復を使わない。試験のために無条件上書き・自動再試行・マージを製品へ追加しない。

## Deep Archive1個

通常データの保存先と分け `${archive_test_root}archive/sample.bin` に64KiBを1個だけarchivedで保存する。index.locationはローカルの受入用DBへ戻しておく。

```sh
private-archive --config "$archive_test_dir/config.toml" cp "$archive_test_dir/sample.bin" "${archive_test_root}archive/sample.bin" --data-class archived
private-archive --config "$archive_test_dir/config.toml" restore --restore-duration 1d --restore-tier Bulk "${archive_test_root}archive/sample.bin"
```

復元要求は1回、日数1、方式Bulk。受付応答だけで合格にしない。操作者が必要時に次を明示実行して新しい状態を確認する。製品へ待機・定期pollingを実装しない。

```sh
private-archive --config "$archive_test_dir/config.toml" reconcile "${archive_test_root}archive/sample.bin"
private-archive --config "$archive_test_dir/config.toml" ls "${archive_test_root}archive/sample.bin" --detail
```

復元完了後に取得し、元サンプルとのcmp/SHA256一致を記録する。要求から最大72時間、再要求なし。上限で未完了なら未検証として記録し、試験データを清掃する。最低180日分の保管料と追加メタデータ、復元コピー、取り出しを見積りに含める。

## 後始末と合格判定

1. GUI/CLIを閉じ、configをローカルindexへ戻す。使用中のS3 indexをデータ操作で削除しようとする保護を迂回せず、保存先を明示変更してから試験indexを削除する。
2. 台帳の試験prefix内だけを `rm ... --recursive --auto-approve` で削除する。試験が開始したmultipartがあれば、その記録済みIDだけをAbortする。小容量単一PUT試験では通常0個。
3. 管理用認証でbucket全体が空であることと、未完了multipartがないことを確認する。試験外のkey/uploadを見つけたら削除せず未達を通知する。
4. 空bucket・IAM role/user・Terraform定義/lockfile/stateを運用へ残す。destroy・一時的な権限拡大・object自動削除Lifecycleは行わない。
5. 3方向内容一致、属性、index往復/条件式、Deep Archive復元後の内容一致、空bucket、数量台帳、見積りの範囲内を受入結果へ記録し、[対応表](FINAL_ACCEPTANCE.md)とTASKSのP3-11を更新する。

この手順は未実施。上記の実結果が揃うまでP3-11全体を完了にしない。

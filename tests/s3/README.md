# Docker S3検証環境

P1-04/P1-05のRust adapter試験の結果・再現command・fixtureの制約は[S3 adapter検証記録](../../docs/S3_ADAPTER_VALIDATION.md)を参照。probeだけの結果と区別する。

P1-06〜P1-08の製品server copy・実manifest/SQLite付き3方向コアの試験と再現手順は [追加検証記録](../../docs/CORE_COPY_VALIDATION.md)。Cargoのfixture試験3件は実行別prefixを使い、終了時に対象objectを削除する。

SeaweedFS OSS 4.53をバージョン・image digest固定で使う。ライセンスはApache 2.0。必要なPUT/GET/HEAD/LIST/Copy/multipartを持ち、単一コンテナで起動できるため採用した。MinIO・LocalStackの契約やアカウントを必要としない。
根拠: [公式リポジトリとDocker手順](https://github.com/seaweedfs/seaweedfs)、[API対応表](https://github.com/seaweedfs/seaweedfs/wiki/Amazon-S3-API)。

これは開発・検証専用で、アプリの配布・実行時依存には含めない。実AWSのIAM・AssumeRole・Deep Archive・Lifecycleの再現を保証しない。とくにRestoreObjectはSeaweedFSの対応表でも未対応であり、P2の復元状態・要求・失敗契約はfake adapter/制御可能なHTTP応答で検証する。実AWSで未確認の項目はそのまま記録する。

## Phase 1の実CLI検証

2026-11-16、機能8項目・障害15項目と256MiB/1GiB/4GiBの実CLI受入を完了した。[最終検証と生データ](../../docs/PHASE1_VALIDATION.md)を参照。再現はfixture起動後に以下を使う。保存済みfixture binaryがなければ先にbuildする。

```bash
cargo build -p private-archive --features test-fixtures --locked
python3 tests/s3/faults.py --binary target/debug/private-archive --report /tmp/archive-faults.json
uv run --script tests/s3/acceptance.py --binary target/debug/private-archive --functional --sizes-mib 1 --report /tmp/archive-functional.json
uv run --script tests/s3/acceptance.py --binary target/debug/private-archive --large --reset-fixture-between-sizes --sizes-mib 4096 1024 256 --report /tmp/archive-large.json
```

容量試験は各段階の空き領域を検査する。`--reset-fixture-between-sizes`はバケット内の全object/未完了uploadが空であることを検査してから、このcompose専用volumeだけを作り直す。各試験は専用prefix/config/indexと固定dummy認証を使い、通常CLIのAWSラッパーを経由しない。通常buildでendpoint引数は利用できない。

## 起動と互換性確認

プロジェクトrootから実行する。必要なのはDocker Composeと、probe実行用のuv。Python/boto3はuvがscript専用環境へ用意し、システムのPythonへインストールしない。

```bash
docker compose -f tests/s3/compose.yaml up -d
uv run --script tests/s3/probe.py
```

- endpoint: `http://127.0.0.1:18333`（ホスト公開はloopbackだけ）
- region: `us-east-1`、path-styleアクセス
- bucket: `private-archive-test`（起動時に作成）
- 固定のダミー認証: access key `private-archive-test`、secret `private-archive-test-secret`
- serverの上限: memory 1GiB、CPU 2。製品CLIのRSS予算とは分ける。
- data: このcompose project専用のDocker named volume。コード・本番データのbind mountなし。

probeはendpointとダミーキーをコード内で固定し、ホストのAWS認証を使わない。実行ごとのprefixとupload IDだけを削除し、既存キー・他のuploadを一括削除しない。失敗は非ゼロ終了する。最大16MiBの本文でfixtureのAPIを確認するもので、private-archiveの実装・大容量性能・100GB対応の検証ではない。

```bash
docker compose -f tests/s3/compose.yaml logs --tail=100 s3
docker compose -f tests/s3/compose.yaml down
```

停止後も試験volumeは残る。試験領域を初期化する場合に限り、次を使う。削除対象はこのcompose project専用のデータである。

```bash
docker compose -f tests/s3/compose.yaml down --volumes
```

## 2026-11-15の互換性確認結果

Docker Engine 28.1.1 / Compose v2.35.1、SeaweedFS 4.53、boto3 1.44.3でprobeが正常終了した。固定image digestはcompose.yamlに記載する。

| 確認項目 | 結果 |
| --- | --- |
| PUT / GET / HEAD / user metadata / CRC32 | 成功 |
| CopyObjectと本文SHA256一致 | 成功 |
| ListObjectsV2のMaxKeysとcontinuation token | 成功 |
| 8MiB×2のmultipart upload / FULL_OBJECT CRC32 / GET | 成功 |
| UploadPartCopy / Complete / 本文SHA256一致 | 成功 |
| UploadPart後のAbort / 残存uploadなし | 成功 |
| 不正CRC32のBadDigest拒否 | 成功 |
| 未作成キーのHEAD 404 | 成功 |
| 今回のprefixのオブジェクト削除 | 成功 |

ほかのchecksum形式・SDK固有の挙動・不正応答・途中切断・誤った権限の403は、このprobeの確認範囲に含めない。これらはP1-14のRust SDK adapter/実CLIとfault injectionで別途確認済み。将来の復元試験ではfixtureの未対応を製品の対応済みと読み替えない。

アプリを接続する検証用ビルドは、`test-fixtures` featureでのみendpoint注入を有効化する設計とする。通常のCLIへ試験用フラグは追加しない。CLI検証は専用config/index/temp領域とダミー認証を使い、実AWS用のprofile/roleラッパーを経由させない。P1-01/P1-04/P1-14で実装済み。

詳細な容量・メモリ・100GB相当の検証と未確認範囲は [転送・検証計画](../../docs/archived/TRANSFER_VALIDATION.md) を参照する。

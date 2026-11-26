# CLI移行・ARM検証

実AWSを使わず、既存SeaweedFSとloopback HTTPだけで検証する。製品の通常ビルドへ検証用endpointを追加しない。設定とDBは一時ディレクトリに限定し、ホストのAWS認証をコンテナへ渡さない。

## ARMバイナリの作成

現在のIntel Macでは、Dockerのamd64 Rustからaarch64-unknown-linux-gnuへクロスコンパイルする。ARM内でのネイティブコンパイル用Dockerfileもあるが、エミュレーションで長時間かかる。バイナリの実行は後述のARM環境で行い、ビルド成功だけを実動作確認としない。

```bash
docker build --platform linux/amd64 -f tests/portability/Dockerfile.cross -t private-archive-cross-arm64:1.99.1 tests/portability
docker build --platform linux/arm64 -f tests/portability/Dockerfile -t private-archive-build-arm64:1.99.1 tests/portability
docker run --rm -d --platform linux/amd64 --name private-archive-p2-cross-builder --cpus 2 --memory 3g --cap-drop ALL --security-opt no-new-privileges -v private-archive-p2-cross-target:/target -v private-archive-p2-arm64-registry:/usr/local/cargo/registry -e CARGO_TARGET_DIR=/target -e CARGO_BUILD_JOBS=1 private-archive-cross-arm64:1.99.1 sleep infinity
docker cp Cargo.toml private-archive-p2-cross-builder:/workspace/
docker cp Cargo.lock private-archive-p2-cross-builder:/workspace/
docker cp crates private-archive-p2-cross-builder:/workspace/
docker cp tests private-archive-p2-cross-builder:/workspace/
docker exec private-archive-p2-cross-builder cargo clean --target aarch64-unknown-linux-gnu -p archive-config -p archive-domain -p index -p sync -p archive-infra -p private-archive
docker exec private-archive-p2-cross-builder cargo build --locked -p private-archive --features test-fixtures
mkdir -p target/arm64
docker cp private-archive-p2-cross-builder:/target/aarch64-unknown-linux-gnu/debug/private-archive target/arm64/private-archive-fixtures
docker stop private-archive-p2-cross-builder
```

ビルド用の名前付きvolumeは再実行用cache。コンテナを閉じても利用できる。再ビルド時は利用者データや認証を含めず、その時点のソースをコピーし直す。コピー元のmtimeが前回buildより古い場合も取り違えないよう、上記では依存クレートのcacheを残して製品クレートだけcleanする。

## 同じDB・設定をMacとLinux ARM64で使う

[S3テスト環境](../s3/README.md)のSeaweedFSを起動してから実行する。ホストのloopback 8333番を18333番へ転送し、ARM側はSeaweedFSのnetwork namespace内の127.0.0.1:8333を使う。両側のfixture namespaceを同じ値に保ち、loopback限定を緩めない。

```bash
uv run --script tests/portability/cli.py --binary target/p2-fixtures/private-archive --arm-binary target/arm64/private-archive-fixtures --report docs/validation/p2-portability.json
```

`--arm-binary`なしではMacだけの運用/移行試験になる。ARMコンテナには一時作業ディレクトリだけを共有し、終了時に削除する。テスト用prefixだけを後始末し、他のS3互換データには触れない。

cfgの保存と相対パス、3方向cp、隠し名、一覧/ソート、再照合、閉じた通常DBの外部バイナリ転送によるlocal→S3→local、外部追加/削除、rmと最後のルート消滅、破損DBの保持と新規保存先への明示再照合を確認する。ELF machine=aarch64、実行環境のLinux/aarch64、追加SQLite/TLS共有ライブラリへの依存がないことも確認する。Deep Archive復元のサービス挙動はこのSeaweedFS試験で保証しない。

## 復元フローと大件数

```bash
python3 tests/s3/workflow_restore.py --binary target/p2-fixtures/private-archive --report docs/validation/p2-workflow-restore.json
python3 tests/s3/concurrency.py --binary target/p2-fixtures/private-archive --report docs/validation/p2-concurrency.json
python3 tests/s3/scale_listing.py --binary target/p2-fixtures/private-archive --report docs/validation/p2-scale-listing.json
CARGO_TARGET_DIR=target/p2 cargo build --locked -p archive-infra --example scale_operations
python3 tests/s3/scale_operations.py --driver target/p2/debug/examples/scale_operations --report docs/validation/p2-scale-operations.json
```

大件数の計測runnerは現在Macの`/usr/bin/time -l`を使い、RSSの単位はbytes。各実行は別プロセスとする。一覧は10項目JSON/順序/件数を逐次検証し、100万件をテスト側でも配列へ保持しない。再照合は実SQLite、rm/restoreは実DiskObjectsとコアの全件解決/事前検査/全件操作を通す。rm/restoreのインデックスは件数だけのfake、実データ操作も本文なしfakeであり、100万件のHTTP要求や本文は生成しない。

結果は[移植性記録](../../docs/PORTABILITY_VALIDATION.md)と[Phase 2検証記録](../../docs/PHASE2_VALIDATION.md)へ実行済み範囲だけを記載する。

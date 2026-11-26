# ビルドcacheとディスク容量

2026-11-23、利用者指示により不要なCargo生成物・Docker image/build cacheを定期的に整理する。配備済みCLI/GUI/.app・fixture/ARM実行物・検証記録・設定/DB・Terraform stateは保持する。

このworkspaceでは `target/bin` や `target/apps` に配備物も置いているため、ルートの `target` 全体を削除する `cargo clean` は使わず、ビルド専用targetかpackageを指定する。現在のMacのbuild cacheは `target/p2`。

```sh
# 自分のビルド/リンク処理が終了し、配備物をコピーしてから行う。
CARGO_TARGET_DIR=target/p2 cargo clean -p private-archive-gui
df -h .
docker system df
docker image prune -f
docker builder prune --filter until=24h -f
```

実行中のARM buildで使うregistry/target volumeと、SeaweedFSのデータvolumeは削除しない。名前付きbuild imageの削除は、それを使うコンテナと受入が終了してから行う。再ビルドが必要になるため、必要なARM実行物を先に `target/arm64` へコピーする。

今回の整理では、途中のGUI package/incremental約5GiB、配備後のpackage約4.6GiBに加え、最後にビルド専用の `target/p2` をcleanして10.1GiBを削除した。ARMビルド生成物891.4MiB、終了済み試験の一時問い合わせDB約249MiB、不要になったGUIクロス/runtime imageとDocker build cache 2.751GBも整理した。最終確認の空き容量は約22GiB。各数値は段階ごとの削除報告で、途中の再生成を含むため単純合計を空き容量の増分とはしない。

GUIの大件数試験は一時DB/一時問い合わせ結果を使い、runner終了時にそれらを削除する。SeaweedFSのコンテナ・image・データvolume、名前付きvolume、配備物は保持した。配備物のhash・検証結果はdocs/validationへ残す。

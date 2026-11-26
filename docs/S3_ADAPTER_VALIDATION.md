# P1-04 / P1-05 S3 adapter検証記録

2026-11-15。対象はinfraの読み取り・保存adapterとsyncの逐次本文port。[転送計画](archived/TRANSFER_VALIDATION.md)に従う。CLIのcp接続・全対象manifest・S3→S3 copy・大容量受入の完了記録ではない。


後続P1-06〜P1-08のS3間コピー・index/manifest・コア接続は [追加検証記録](CORE_COPY_VALIDATION.md) を参照。本書の39件はP1-04/P1-05時点の結果。
## 構成と境界

- `S3Store::from_env`がSDK標準設定・認証チェーンを使う。region/profile/roleをアプリへ固定しない。SDK型・クライアント・認証情報はinfra内に保持する。CLIの通常起動にはまだ組み込まず、ローカル参照から認証解決しない。
- 共有設定とS3クライアントへ`max_attempts=1`を明示。aws-configが内部STS用ProviderConfigへ同じretry設定を渡すことも同梱ソースで確認した。通常buildでは独自endpointを受け付けない。
- `test-fixtures` featureの`S3Store::for_test`だけがloopback endpoint・path-style・固定ダミー認証を設定する。標準profileを読まず、`test:<endpoint>` namespaceを使う。実AWSの`aws` namespaceを渡すと送信前にエラー。CLIのfeature転送はP1-11で追加する。
- HEAD/GETは指定objectのみ。LISTは指定prefix・MaxKeys=1000・先読みなし。深さ/ignoreを1件ずつ適用し、不正な継続tokenを拒否する。LISTの照会時刻はページ取得時に固定し、表示時刻で更新しない。任意メタデータを得るための追加HEADをしない。
- 保存は16MiB以下が単一PUT、それ以上は16MiB multipart・同時最大4要求。待機キューは持たず、最大4送信+1組立ての80MiBをadapter本文の上限とする（設計予算の最大6本文96MiB以内）。各本文はBytesの共有参照で渡す。全fileをcollectせず、本文のretry copyを作らない。完了part情報は10,000件・4MiBで制限する。
- PUT/partごとのCRC32とmultipart全体のFULL_OBJECT CRC32を送信し、Completeに全体サイズも渡す。結果はSDKの解析済み応答で判定する。HTTP 200のError XMLは成功にしない。
- 元属性の保存・読戻し・復元はP1-09で接続する。管理index・一時manifest・CLIの成功表示はP1-08/P1-11へ残る。

## 進捗・失敗・取消

接続10秒、本文の無進捗120秒、API操作最大15分。GET本文全体へ短い固定期限を置かない。SDKのread timeoutは送信開始から応答先頭までを含むため、PUT/UploadPartではそのtimeoutを明示解除し、16KiBずつHTTPへ渡す共有Bytesの消費進捗と応答待機を監視する。元Bytesとframeのポインタ一致・正確なサイズをテストした。HTTPへの引渡し時刻の監視であり、TCP ACKの計測ではない。SDK既定の別のstalled-stream条件は無効化し、この監視とAPI上限を使う。

パート失敗は共有通知でproducerの本文待機を起こし、新規送信を止める。取消の最初の時刻を共有し、発行済み要求の確認30秒+対象upload IDへのAbort最大15秒+診断最大5秒とする。PUT/Complete等の発行後に応答を失った場合はUnknown。確認できた成功は取消で取り消さない。Abortは応答不明・失敗も記録し、同じsessionで再呼出しされても再送しない。session dropだけではAbortを保証しない。CLIを含む50秒の受入・S3→S3の後始末はP1-10/P1-14に残る。

## 環境と実行

macOS 12.7.6 / x86_64、Rust 1.99.1。aws-sdk-s3 1.159.0、aws-config 1.13.2、aws-sdk-sts 1.126.0、aws-smithy-runtime 1.16.3。依存はCargo.lockで固定。HTTP/TLSのfeature treeはhyper/rustls/aws-lc-rs。S3 adapterを含むtest executableの`otool -L`はSecurity/CoreFoundation/libiconv/libSystemのみで、追加の共有ライブラリはない。C/assemblyのビルド環境は必要。古いmacOS linkerがaws-lcのSHA3 assemblyのcompact-unwind警告を出したが、リンクと全試験は成功した。ARM/Linuxの実機検証は未実施。

Dockerのimage digest・固定ダミー認証・volume構成は[fixture手順](../tests/s3/README.md)を参照。試験では`http://127.0.0.1:18333`の`private-archive-test`だけを使い、実行別prefixを削除する。

```bash
cargo test --workspace --all-features --locked -- --include-ignored
cargo fmt --all -- --check
cargo clippy --workspace --all-features --all-targets --locked -- -D warnings
```

最初のcommandはDocker起動が必要。通常の`cargo test --workspace`はDockerを要求しない。fixture feature付きの故障HTTP試験だけなら`cargo test -p archive-infra --features test-fixtures --lib`で実行できる。

## 検証結果

既存26件と新規13件、計39件成功。書式チェックと全feature/全targetのClippy（警告をエラー扱い）も成功。主な観測:

| 対象 | 結果 |
| --- | --- |
| 標準profile・regionの選択 | 別process・専用設定・dummy keyで確認。既定profileを選ばず、環境/設定のretry値9/12よりアプリの1を優先 |
| 独自endpointとnamespace | 通常設定のS3/STS endpointを拒否。fixtureへaws namespaceを渡すと送信0回 |
| HEADの403 / 404 / 500 | Permission / NotFound / Unavailableを区別。各送信1回、request ID保持 |
| LIST | 初回nextまで送信0、1ページ消費後だけ次ページ、ignore/深さを適用 |
| opaque key | 制御HTTPサーバーで連続`/`・`.`・`..`・`%`・`?`・`#`が正しいencoded pathに保持されることを観測 |
| GETの短縮・誤checksum | エラー終了、既存local宛先を保持、一時file削除 |
| PUTのclass / CRC / 応答喪失 | DEEP_ARCHIVE headerとCRCを確認。応答なしはUnknown、再送なし |
| UploadPart失敗 | 入力待機中も停止、Complete未発行、今回のupload IDだけAbort |
| Abort失敗 | 元の失敗を保持してcleanup warning、再呼出しでも再送なし |
| Completeの200+Error XML | エラー終了、成功結果なし、対象uploadをAbort |
| 100GB / 100GiBのplanner | 5,961 / 6,400部、64bit offsetの連続性と終端、10,000上限を確認。本文生成/転送なし |

Docker上のupload→downloadでは独立したSHA256比較とlocal宛先の上書きを確認した。1MiBケースではS3側の既存objectの上書きも確認した。

| bytes | 保存方法 | part数 | 内容 |
| --- | --- | --- | --- |
| 0 | PUT | 0 | SHA256一致 |
| 1,048,576 | PUT | 0 | SHA256一致、S3上書きも成功 |
| 16,777,216 | PUT | 0 | SHA256一致 |
| 16,777,217 | multipart | 2 | SHA256一致、FULL_OBJECT CRC32照合成功 |
| 33,554,449 | multipart | 3 | SHA256一致、FULL_OBJECT CRC32照合成功 |

SeaweedFSの単一PUTではGETにchecksum種別がないためUnknownとし、size/EOF検証+verification warningになることを確認した。FULL_OBJECTと推測して埋めない。SDKのchecksum検証はWhenSupportedを明示したままとする。multipartではFULL_OBJECT種別が返り、製品の照合も成功した。

SeaweedFSは連続スラッシュを正規化するため、そのキーのidentity保持はDockerだけで合格としない。wire上の保持は制御HTTPサーバーで別に確認した。Dockerの本文往復は通常の階層キーを使う。失敗した初回試験の専用objectも削除し、最終確認で`rust-adapter-` prefixのobjectと未完了uploadはともに0件だった。

256MiB/1GiB/4GiB、CLI RSS、100GB相当の本番scheduler/fake sender試験はP1-15。実AWSのIAM/AssumeRole/Deep Archive/Lifecycle/TLS通信は未検証。この結果を製品CLIや100GB実転送の受入成功とはしない。

一次資料: [SDK HTTP構成](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/http.html)、[SDK retry設定](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/retries.html)、[TimeoutConfigのTTFB定義](https://docs.rs/aws-smithy-types/1.9.4/aws_smithy_types/timeout/struct.TimeoutConfigBuilder.html#method.read_timeout)、[ByteStream::map](https://docs.rs/aws-smithy-types/1.9.4/aws_smithy_types/byte_stream/struct.ByteStream.html#method.map)。

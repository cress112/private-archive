# CLIの導入と初回実行

CLIの`cp`はlocal→S3 / S3→local / S3→S3、再帰・深さ・ignore・保存クラス・新規/上書きに対応する。設定はCLI/GUIで共有する。Phase 2では`cfg`・`ls`・明示的な再照合・S3インデックス取得/保存・複数URIの復元要求・確認付き削除も利用できる。[検証記録](PHASE2_VALIDATION.md)を参照。GUIも利用できる。[GUIの起動・macOSアプリ・操作手順](../crates/gui/README.md)を参照。保存済み通常版target/bin/private-archiveもPhase 2版へ更新済み。

## ビルドと起動

Rust toolchainはCargo.tomlのrust-versionに従う。本体はSQLite/TLSをビルド時に取り込み、実行時のAWS CLI・sqlite3・Dockerを必要としない。認証を解決してから本体を起動する起動スクリプトにはAWS CLI v2が必要。OS標準ライブラリは使用する。

```bash
cargo build -p private-archive --locked
./target/debug/private-archive --help
./target/debug/private-archive cp --help
```

リリース用には`cargo build -p private-archive --release --locked`を使える。今回の実転送受入はx86_64 macOSの開発ビルドで実施し、release/ARM/Linuxの実機検証と区別する。バイナリをPATHへ配置するか、ラッパーで`PRIVATE_ARCHIVE_BIN`へ絶対パスを指定する。今回保存した通常buildは`target/bin/private-archive`、fixture buildは`target/fixtures/private-archive`。

設定の既定パスは`~/.private-archive/config.toml`。未作成でも既定値で起動し、暗黙に設定ファイルを作らない。

```toml
ignore = ["*.tmp", ".DS_Store"]
[index]
location = "~/.private-archive/index.db"
[copy]
data-class = "standard"
[restore]
tier = "Bulk"
[logging]
level = "info"
```

優先順位は組み込み値→設定→実行時指定。`--config PATH`、`--index-location PATH_OR_URI`で変更できる。コピー自体は管理インデックスと独立したmanifestと今回の実ストア観測で実行する。

## 共有設定の表示・作成・編集

```bash
private-archive cfg
private-archive cfg init
private-archive cfg get logging.level
private-archive cfg set logging.level warn
private-archive cfg set index.location 's3://cress-archived-data/index.db'
private-archive cfg set copy.data-class archived
private-archive cfg set restore.tier Bulk
private-archive cfg set ignore '.DS_Store,*.tmp,a\,b'
private-archive cfg unset logging.level
private-archive --log-level debug cfg
```

`cfg`は全5項目の有効値と出所（built-in/config-file/runtime）を表示する。`get`は1項目の値だけをTOMLの値表記で出すため、文字列は引用符付き、ignoreは配列となる。JSON出力ではない。読み取りで設定ファイル・インデックスを作成しない。

`init`は既定値をコメントで示したTOMLを作り、既存ファイルは上書きしない。`set`と`unset`は作成済みのファイルだけを編集する。保存先は`--config PATH`で選び、設定内の相対indexパスはそのファイルの親を基準にする。実行時上書きは保存しない。`unset`は明示値を取り除くので、実行時上書きがなければ組み込み値へ戻る。

`logging.level`と`--log-level`はerror/warn/info/debug/trace、既定info。結果stdoutと終了コードはログ抑制で変えない。不正なキー/値/設定は終了2、作成・読書き・ロック競合の失敗は終了1。保存競合で待ち続けず、再実行は操作者が明示的に行う。

設定操作だけでは、S3インデックスを含めて保存先を開いたり、AWSへ問い合わせたりしない。index.locationの変更でインデックス本体を移動・変換しない。

## ローカルインデックスの一覧

```bash
private-archive ls
private-archive ls s3://cress-archived-data/archive/file
private-archive ls s3://cress-archived-data/archive/ --recursive
private-archive ls s3://cress-archived-data/archive/ --recursive --recursive-depth 2 --detail
private-archive ls s3://cress-archived-data/archive/ --recursive --sort size-asc,name-desc --json
```

URIを省略すると、記録済みオブジェクトから導出したバケットのルートを表示する。再帰一覧にはURIを指定する。非再帰では完全一致するオブジェクトを優先し、なければディレクトリ相当またはルートを1件返す。存在しない対象・空の範囲・全件ignoreは終了1、JSONの0件結果は`[]`。除外した既存記録を削除しない。

通常は1行1URI。制御文字・バックスラッシュを人向け出力ではescapeし、JSONでは元のURI文字列を保つ。詳細はタブ区切りの基本10項目、JSONも全10キーを出し、不明値はnull。日時はUTC RFC3339、バイト数は整数、権限は8進数文字列。仮想ディレクトリ/ルートのサイズ等は合算しない。復元期限が過去でも最後に記録した状態を表示する。

ソートの6値はname-asc/name-desc、size-asc/size-desc、uri-asc/uri-desc。カンマ区切りを左から優先し、不明値は方向によらず最後、同値はURI昇順。名前は末尾の成分で、大小文字を区別する文字順。`size-asc,name-desc`はサイズ昇順・同サイズなら名前降順。空の条件・未知値・同じ属性の重複は入力エラー。通常/詳細/JSONで同じ全体順を使い、ページ内だけを並べ替えることはしない。

初回・詳細・JSON・再帰でもS3 APIを発行せず、認証情報を解決しない。S3保存先はローカルworking copyを参照し、なければ空の写しだけを初期化する。取得/再照合は明示操作の経路で行い、一覧で鮮度確認や不足値の追加HEADを行わない。破損時も自動再構築しない。

## 明示的な再照合

```bash
private-archive reconcile s3://cress-archived-data/archive/file
private-archive reconcile s3://cress-archived-data/archive/ --recursive
private-archive reconcile s3://cress-archived-data/archive/ --recursive --recursive-depth 2
private-archive ls s3://cress-archived-data/archive/ --recursive --json
```

reconcileは指定した実ストアの状態を照会してインデックスへ反映し、観測・削除・除外の件数を表示する。一覧は別のlsで参照する。非再帰は完全一致の1件、バケット/配下は--recursiveを指定する。古いインデックスから照会対象を選ばない。

正常に確認した空の範囲は成功とし、その範囲内の古い記録を除去できる。全件ignoreは終了1で、削除照合へ進まない。通信/権限/インデックス反映の失敗や取消では取得済み反映を残し、未取得の記録を不存在として削除しない。自動リトライ・自動再照合は行わない。個々の再帰ignoreはdebug、直接指定はwarn、確認済み除外通知件数はinfo。

ローカル/S3いずれのindex.locationでも使える。実ストア照会の実行検証はloopbackのfake/fixtureだけを使い、実AWSは最終フェーズまで行わない。

## S3に保存するインデックス

index.locationをS3 URIへ設定すると、cp・rm・restore・reconcileの開始時に通常のDBファイルを取得して共有作業DBへ取り込み、その実行で変わった場合だけ終了時に1回保存する。取得時に確認したETagへのIf-Match、未作成ならIf-None-Matchで保存し、競合や応答不明でも再送・自動マージしない。本文はStandardで保存する。copy.data-classは適用しない。

cpでインデックスの取得・取込・保存に失敗した場合はwarningを表示し、データ操作の成否は独立して返す。reconcileの必須処理が完了しない場合は終了1。途中失敗・Ctrl+Cでも、取得が正常に完了していれば確認済みの変更の保存を停止予算内で試みる。取得失敗・破損で保存基準を確立できない場合は上書きしない。

S3側の本文はローカルと同じ通常のDBファイルで、専用形式や必須sidecarはない。全利用プロセスを閉じて整合したDBを外部ツールでS3へ置く、またはS3からローカルへ取得して、index.locationを切り替えれば使える。稼働中のWALを無視してmainファイルだけをコピーしない。設定変更だけでは移動・変換しない。

転送最適化用の変更しないsnapshotとETagの対応は、DBの外の所有者専用キャッシュへ保存する。変更済みworking.dbを304応答で再利用せず、次の実行で前回未送信分を正しいものとして保護しない。通常lsは手元の写しを表示するだけなので、他端末の変更を反映したい場合は明示的にreconcileする。

現在のインデックス転送は単一PUTを使い、5 GiBを超えるsnapshotは明示エラーにする。大容量データ本体のmultipart対応とは別の上限。使用中S3インデックスのデータ操作からの保護と、多重実行時の取込/条件付き保存競合は検証済み。移行・100万件の結果は[Phase 2検証記録](PHASE2_VALIDATION.md)で区別する。

## 復元要求

```bash
private-archive restore --restore-duration 7d s3://cress-archived-data/archive/file
private-archive restore --restore-duration 1w --restore-tier Standard s3://cress-archived-data/photos/ --recursive --recursive-depth 2 s3://cress-archived-data/video.zip
```

期間は実行時必須の正整数d/wで、1wは7日。時間表記や切上げは行わない。方式は設定restore.tier、既定Bulk。期間・方式・共有設定のオプションは最初のURIより前に置く。再帰・深さは直前のURIだけへ適用し、次のURIへ引き継がない。隠し名も既定で対象となり、除外は設定ignore/--ignoreで指定する。

全URIを今回の実ストアで照会し、独立した一時manifestで重複排除してから全件を事前検査する。URIの入力順・各URI内キー昇順で1件ずつ要求し、重複は最初のURIの順序を使う。重複の後の照会で復元中等と判明した情報を捨てない。インデックスの古い状態から復元可否を判断しない。

復元中・復元済み・即時取得可能な対象があれば、復元要求を0件のまま全体を終了1にする。不明な対象だけ追加照会し、判定不能・照会失敗も要求0件。完全一致の不存在、空の再帰範囲、全件ignoreも終了1。受付が確認できた対象はURI/日数/方式をstdoutへ逐次表示し、インデックスへ反映を試みる。復元完了までは待機せず、自動ポーリングしない。

要求開始後の失敗・取消では次の要求を止め、受付済みを取り消さない。応答不明を受付成功にせず、再送しない。事前照会後の状態変化でHTTP 200（既に復元済み）が返った場合も新規受付として表示しない。インデックス異常はwarningとして扱い、要求自体の成否を返す。状態を確認するときは操作者が明示的にreconcileを実行し、その後lsで表示する。

## 確認付き削除

```bash
private-archive rm s3://cress-archived-data/archive/file
private-archive rm s3://cress-archived-data/archive/ --recursive --recursive-depth 2
private-archive rm s3://cress-archived-data/archive/ --recursive --ignore '*.tmp' --auto-approve
```

1 URIを指定し、非再帰は完全一致、配下は--recursiveを明示する。バケット全体にも--recursiveが必要。対象は今回の実ストアから列挙し、ignoreと深さを適用して全件解決する。隠し名も既定で対象となる。完全一致の不存在・空の再帰範囲・全件ignoreは終了1。

--auto-approveがなければstdinが端末であることをS3照会/インデックス取得前に要求する。パイプ・ファイル・/dev/nullのyesは読み取らず拒否する。端末ではURI・対象件数・合計サイズをstderrへ表示し、全体で1回だけ確認する。合計不明はunknownと表示し、0へ置き換えない。yesだけを承認し、別の回答・EOFは再入力せず終了1。ログを抑制しても確認表示は行う。stdoutのリダイレクトは利用できる。

--auto-approveならstdinを読まず、確認も表示しない。削除はキー昇順で1件ずつ行い、確認できた削除URIをstdoutへ出し、インデックスの該当記録を削除する。最後の記録がなくなれば一覧のルートも消える。ignore・未走査の深さ・指定範囲外の既存記録を削除照合しない。

最初の失敗・Ctrl+Cでは次の削除要求を止める。応答不明を削除成功にせず、再送・確認目的の追加HEAD・ロールバックを行わない。確認済みの削除結果は表示・インデックス反映を試みる。インデックス異常はwarningとし、データ削除の成否を変えない。

<a id="aws-の準備実awsへの適用は未実施"></a>

## AWS の準備

[Terraform定義](../terraform/environments/main/README.md)は構文・provider整合まで検証済み。利用者からIAMユーザ・ロールの作成と共有プロファイルの設定済みの報告を受けた。実AssumeRole、3方向コピー、インデックス往復、GUI再照合は成功した。Deep Archiveを含む現在地は[実AWS検証記録](AWS_VALIDATION.md)を参照。初回設定では、操作者がAWSコンソールで専用ユーザのアクセスキーを作成し、共有認証ファイルへ登録する。以下は形式の例で、値は実物へ置き換える。秘密値をプロジェクト・ドキュメント・Terraformへ保存しない。

`~/.aws/credentials`:

```ini
[private-archive-user]
aws_access_key_id = REPLACE_WITH_CONSOLE_ISSUED_KEY_ID
aws_secret_access_key = REPLACE_WITH_CONSOLE_ISSUED_SECRET
```

`~/.aws/config`:

```ini
[profile private-archive]
role_arn = arn:aws:iam::ACCOUNT_ID:role/private-archive-role
source_profile = private-archive-user
region = us-east-1
```

専用sourceユーザはこのroleへのAssumeRoleだけを許可する。**本体はSDKの標準認証チェーンを使い、プロファイル名・ロール名を指定しない。** `private-archive` は起動スクリプトが認証解決に使うプロファイル名であり、本体へ渡す設定ではない。

ラッパーはAWS CLI v2の [`configure export-credentials`](https://docs.aws.amazon.com/cli/latest/reference/configure/export-credentials.html) で共有config/credentialsから認証情報を解決する。このrole profileではAWS CLIがAssumeRoleを実行し、アクセスキー・秘密キー・セッショントークンを取得する。SDK側にAssumeRoleを任せたり、`GetSessionToken`だけでS3操作したりする構成ではない。

CLIの起動例（プロジェクトルート）:

```bash
./scripts/private-archive.sh reconcile s3://cress-archived-data/ --recursive
./scripts/private-archive.sh ls s3://cress-archived-data/ --recursive
```

GUIも同じ解決処理を使う:

```bash
./scripts/private-archive-gui.sh
# macOSでは同じ認証ラッパーを同梱した.appを開くことも可能
open 'target/apps/Private Archive.app'
```

`PRIVATE_ARCHIVE_AWS_PROFILE`でラッパーが使う外部プロファイルを変更できる（未指定は`private-archive`）。`AWS_CONFIG_FILE`/`AWS_SHARED_CREDENTIALS_FILE`による共有ファイルの選択も可能。リージョンは`AWS_REGION`→`AWS_DEFAULT_REGION`→選択プロファイルのregionの順に解決する。AWS CLIはPATHまたはmacOSで一般的な`/usr/local/bin`・`/opt/homebrew/bin`から探す。

CLI/GUIの起動スクリプトと.appのnativeランチャーは、プロジェクト内の共通の [private-archive-session.sh](../scripts/private-archive-session.sh) を実行する。親の直接認証・role/web/container認証・endpoint指定を外し、選択プロファイルを解決してから、認証情報とリージョンを**環境変数**で本体へ渡す。本体へ`AWS_PROFILE`/`AWS_DEFAULT_PROFILE`は渡さず、親shellの環境や共有ファイルも変更しない。認証情報をコマンド引数や一時ファイルへ書かず、取得結果をshellコードとしてevalしない。解決に失敗した場合は本体を起動しない。

起動・認証処理はプロジェクト内の`scripts/`を正本とする。`private-archive.sh` / `private-archive-gui.sh`は実行用の入口で、sourceしない。本体はこのプロジェクトの`target/bin/`を既定とし、必要なら`PRIVATE_ARCHIVE_BIN` / `PRIVATE_ARCHIVE_GUI_BIN`で変更する。認証処理を他の場所へ複製しない。

認証解決はラッパーの起動時に行うため、その段階でSTSやcredential_processが動く場合がある。本体の`ls`やGUIの通常閲覧は、従来どおりS3を照会しない。認証解決なしでローカル参照だけを行う場合は`./target/bin/private-archive ls`や`./target/bin/private-archive-gui`で本体を直接起動できる。

渡した認証情報はそのプロセスで固定され、期限切れ後に本体が名前付きプロファイルへ戻って更新することはない。CLIは次の実行時、GUIは再起動時にラッパーが再解決する。このIaCのrole sessionは最大1時間。MFA入力が必要なプロファイルは端末からラッパーを実行して認証するなど、入力可能な外部認証経路を使う。実AssumeRoleと必要なS3操作は確認済み。[実AWS検証記録](AWS_VALIDATION.md)を参照。

## cp の使い方

```bash
private-archive cp ./file s3://cress-archived-data/archive/file
private-archive cp ./directory/ s3://cress-archived-data/archive/ --recursive
private-archive cp ./directory/ s3://cress-archived-data/archive/ --recursive --recursive-depth 1
private-archive cp s3://cress-archived-data/archive/file ./restored-file
private-archive cp s3://cress-archived-data/archive/ s3://cress-archived-data/second/ --recursive
private-archive cp ./file s3://cress-archived-data/archive/file --data-class archived
```

`standard`が既定。`archived`はS3 Deep Archiveで、復元要求はrestoreで実行する。復元前・復元中のオブジェクトを取得/コピーしようとするとエラーにする。Deep Archive固有の実AWS挙動・課金・復元は今回のDocker試験では確認していない。

再帰は指定ディレクトリ/prefixの中身を配置し、元のディレクトリ名を余分に追加しない。深さの直下は1。`--recursive-depth`は`--recursive`と併用し、0は入力エラー。非再帰のS3宛先がbucket root/末尾slashなら元basenameを追加する。

再帰では`.DS_Store`・`.git/`を含む隠し名も既定で対象となる。除外には設定ignoreまたは`--ignore`を明示する。`--ignore '*.tmp,cache/'`は設定のリスト全体を置換し、`--ignore ''`で解除する。`\,`は文字としてのカンマ、`\\`は文字としてのバックスラッシュ。それ以外のescapeはgitignore記法へ渡す。コピー元と配置先の片方でも該当すればskipする。個々の再帰除外はdebug、直接指定の除外はwarn、実際に確認した除外通知件数はinfoで表示する。ignoreされたsymlinkはskip、それ以外のsymlink/FIFO/socket/deviceはエラー。空ディレクトリ・圧縮・集約・owner保存は行わない。

## 成否・途中失敗・取消

- 全コピー元と対応する宛先を事前照会し、独立したdisk manifestをsealしてから本文転送する。後半の照会・衝突・manifest保存の失敗では本文転送0回。
- 最初の要求操作失敗で新規処理を止める。自動リトライ、途中再開、全体rollbackは行わない。再実行は各ファイルの先頭から。
- downloadは同じディレクトリの一時ファイルでサイズ・EOF・可能なchecksumを検証してから置換する。元のmtime/creation/permissionsはbest effort。取得不能・OS非対応・復元失敗はwarningとし、本文成功を覆さない。
- インデックスは実データの写し。破損・利用不能・反映失敗をwarningにしてコピーを続行し、破損ファイルを空に置換しない。インデックスだけで転送の実行判断をしない。写しを作り直す場合は、以下のように新しい保存先を明示して再照合する。
- 成功ごとに元URI・宛先URI・bytesをstdoutへ1行出力する。診断と完了件数の進捗はstderr。途中失敗でも確定成功の表示・index反映を試みる。
- PUT/Copy/Complete等の応答を失った場合は`outcome=Unknown`で終了1（最初の停止原因がCtrl+Cなら130）。未変更と断定せず、成功として記録しない。
- Ctrl+Cで新規file/partを止め、最初の停止時刻から最大50秒の共通予算で要求の収束・今回IDだけのAbort・index/表示/一時領域の後始末を試みる。2回目のCtrl+Cは強制終了し、後始末を保証しない。

全CLIの終了コード: 0=成功/warningのみ、1=要求操作失敗/Unknown/出力失敗/対象0件、2=入力・設定不正、130=Ctrl+C。cp・ls・rm・restoreは全件ignoreでも1となる。reconcileは正常に照会した空範囲なら0、全件ignoreなら1で削除照合しない。最初の失敗または取消の原因を保持し、ログ抑制で結果と終了コードを変えない。強制終了や後始末失敗で残ったmanifestは自動再開しない。残存した場合は診断に一時領域の場所を表示する。

使用中のS3インデックス自体をcpの元/宛先・rm/restoreの対象に含めると、ignore適用後の全件検査で終了2となり、データ操作を開始しない。自動除外はしない。再帰範囲に含めたくない場合はignoreを明示する。インデックス保存先adapterによる内部のDB取得・条件付き保存はこのデータ操作とは別に行う。

## 破損した写しと端末移行

破損の診断には終了コードとstderrのstage/code/causesを使う。元ファイルを保持し、新しい保存先を選んで必要な実ストア範囲を明示的に再照合する。再照合後にlsで確認する。通常lsだけではS3の写しを取得しない。

```bash
private-archive cfg set index.location '~/.private-archive/new-index.db'
private-archive reconcile s3://cress-archived-data/archive/ --recursive
private-archive ls s3://cress-archived-data/archive/ --recursive --json
```

インデックスを別端末へ移す際は、共有するCLI/GUIをすべて閉じてからDBファイルをバイナリ転送する。稼働中WALのmainファイルだけをコピーしない。転送したDBをindex.locationへ指定すれば、記録とqueried_atをそのまま参照できる。設定ファイル内の相対保存先は設定ファイルの親基準で解決される。新端末の認証プロファイルとラッパーの実行ファイルパスは、その端末で設定する。

ローカル→S3は、閉じたDBを外部の`aws s3 cp`等で通常オブジェクトとして保存し、index.locationをそのS3 URIへ変更する。逆方向は外部ツールで通常DBをダウンロードし、ローカルパスへ設定し直す。製品の実行時にAWS CLIは不要。ETag/hashの転送メタデータを移す必要はない。S3保存先への切替直後のlsは手元の写しだけを読み、必要ならreconcileを明示して取得する。

条件付き保存の競合では自動再送・自動マージ・前回未送信分の再生を行わない。データ操作の結果はその実行の出力で確認し、次の明示的なreconcileで実ストアから写しを作る。追加した新規保存先を運用に使う場合だけcfgで保存する。`--index-location`による一時指定は設定ファイルを変更しない。

## Docker での再現

実AWSの認証・バケット・Terraform適用を使わずに、[fixture手順](../tests/s3/README.md)とPhase 1検証記録のcommandを使う。endpoint注入は`test-fixtures` buildだけの機能で、明示config/indexと固定dummy認証を必須にし、namespaceも実AWSと分ける。通常buildのCLIにはendpoint注入機能を含めない。

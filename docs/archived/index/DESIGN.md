# index 設計

2026-11-22、P2-02/P3-02の具体方式を[Phase 2・3実装設計](../PHASE2_3_DESIGN.md)に確定した。以下はPhase 1からの段階的検討記録を含み、残論点の現行解決は同実装設計を優先する。
全体設計: [../../../DESIGN.md](../DESIGN.md)。要件: [REQUIREMENTS.md](REQUIREMENTS.md)。2026-11-15、P0-03のローカルインデックス設計を確定。P1-07で以下のport/adapterを実装し、同梱SQLite 3.54.1で検証した。詳細は [コピーコア・インデックス検証](../../CORE_COPY_VALIDATION.md)。S3上のDB取得・書戻し・多重実行の具体方式はP2-02へ残す。2026-11-20に利用者向け契約は、S3書戻しは変更時に終了時1回、途中失敗・Ctrl+C時も予算内で試行、競合時は無条件上書きせず、その実行内で通知して終え、自動マージ・再試行なしと確定した。質問19の追加指示により、未送信分の永続管理・専用復旧は設けない。更新系はその実行で取得した実ストア情報だけで実行判断する。保存先切り替えだけ・自動変換なし・破損時の明示手順は要件確定済み。

## DBの選定とadapter

SQLiteを`rusqlite`の`bundled`・`backup`構成で静的に取り込む。rusqlite 0.43.0 / libsqlite3-sys 0.40.0をlockfileへ固定し、snapshotのdeadline監視用にhooksも有効にする。SQLite本体は3.51.3以降の修正版を選び、実際の組込バージョンを検証する。OS付属SQLiteや外部sqlite3コマンドを実行時依存にしない。

| 観点 | SQLite | redb 4.6.0 | 採用判断 |
| --- | --- | --- | --- |
| 可搬形式 | OS・CPU間で共通の形式を公式に定義 | 可搬性・対応版の確認が別途必要 | 長期運用で移動条件を明文化しやすいSQLite |
| 複数プロセス | WALでreaderとwriterが並行、writerは1つ | MultiWriterで複数プロセスが開けるがwrite transactionは1つ | 両方が候補。redbを非対応とはしない |
| 範囲検索・照合 | 複合索引・TEMP表・短いtransactionを利用可能 | ordered tableと独自の範囲照合が必要 | SQLiteで照合・snapshotの構成をそろえる |
| 実行時依存 | bundledで追加共有ライブラリ不要 | Rustへ組込可能 | pure Rustを必須にしない既存要件を適用 |

根拠: [SQLite可搬形式](https://sqlite.org/onefile.html)、[WAL](https://sqlite.org/wal.html)、[WAL不具合修正](https://sqlite.org/releaselog/3_51_3.html)、[rusqlite bundled](https://github.com/rusqlite/rusqlite)、[redb MultiWriter](https://docs.rs/redb/4.6.0/redb/enum.ConcurrencyMode.html)。

`index`はIndexRead/IndexWrite/IndexSnapshotのportと問い合わせ・更新の意味だけを所有し、SQL・スキーマ・rusqlite型は`infra::sqlite`に閉じ込める。adapterの変更でsyncやCLIの判断を変更しない。DB詳細は独自のOperationErrorへ変換する。

## 保存先非依存の境界（2026-11-22、D-07確定）

indexの公開APIは論理query/update/reconcileと整合したファイルの入出力を表し、S3・ETag・条件付きHTTP要求・キャッシュ配置を所有しない。SQLite等のファイル形式、読込/生成/検査はinfraのDB adapterに閉じる。IndexSnapshotはDB非依存のportであり、redb等も同じ契約のadapterとして追加する。

保存先portはsync側に置き、infraの保存先adapterが不透明なファイルを取得・保存する。ローカルは設定DBを直接開く。ETagはS3 adapterの転送用情報で、objects.version_token等の実データ観測情報とも区別し、DB本体へ埋め込まない。DB adapterへはローカルに利用可能な整合ファイルを渡す。

S3保存の本文は通常のDBファイルと同じ形式。正常終了・整合済みのローカルDBをaws s3 cp等でS3へ置き、逆にS3からダウンロードしたDBをlocalへ置いた場合も、保存先設定の変更だけで利用できる。移動にキャッシュ情報/ETagを伴わせる必要はない。取得可能なタイミングは全体要件を維持する。保存先転送の詳細は[全体設計](../DESIGN.md)を正本とする。

## ファイルと識別

- 既定パスは`~/.private-archive/index.db`。設定の`index.location`で変更する。設定内の相対パスは設定ファイルの親、実行時指定の相対パスはCWDから解決する。`~`は先頭の`~/`または単独の`~`だけを展開し、環境変数・`~user`を暗黙展開しない。
- 管理するのはリモートオブジェクトだけ。元のローカルパスやバケットだけの登録を追加しない。
- 主キーは`(namespace, scheme, authority, object_key)`。実S3のnamespaceは`aws`、schemeは`s3`、authorityはバケット。キーはUTF-8 bytesをBLOBで保存し、大小文字・連続スラッシュ・特殊文字を変換しない。認証プロファイルは識別に含めない。模擬S3はendpoint別namespaceで分離する。
- S3インデックスのcache配置・切替はP2-02で決める。Phase 1でS3のindex.locationを受けた場合は未対応の保存先としてwarningを返し、代わりのローカル保存が成功したように扱わず、cp自体は続行する。cfgの値検証とadapterの段階的な能力対応は区別する。

## スキーマv1

以下はadapter内部のDDL。`PRAGMA user_version=1`、`application_id=0x50415243`（PARC）で識別する。未知の版・別アプリのDBはエラーにし、自動変換・破棄をしない。

```sql
CREATE TABLE metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    revision INTEGER NOT NULL CHECK (revision >= 0)
);
INSERT INTO metadata(singleton, revision) VALUES (1, 0);
CREATE TABLE objects (
    namespace TEXT NOT NULL,
    scheme TEXT NOT NULL,
    authority TEXT NOT NULL,
    object_key BLOB NOT NULL,
    size_bytes INTEGER CHECK (size_bytes >= 0),
    data_class TEXT,
    restore_state TEXT,
    restore_expiry_seconds INTEGER,
    original_mtime_seconds INTEGER,
    original_mtime_nanos INTEGER,
    original_creation_seconds INTEGER,
    original_creation_nanos INTEGER,
    unix_mode INTEGER,
    queried_seconds INTEGER NOT NULL,
    queried_nanos INTEGER NOT NULL,
    store_modified_seconds INTEGER,
    version_token TEXT,
    checksum_algorithm TEXT,
    checksum_type TEXT,
    checksum_value TEXT,
    revision INTEGER NOT NULL,
    PRIMARY KEY (namespace, scheme, authority, object_key)
) WITHOUT ROWID;
```

サイズは共通型ではu64、DBでは符号付き64bitへ検査付き変換する。範囲外を丸めない。未知値はNULL、時刻はUTC秒+nanos。enum表現はadapterで版管理する。バケット一覧は主キーの`DISTINCT namespace, scheme, authority`相当をページ取得し、バケット専用テーブルを作らない。最後のobject削除でルートも消える。

## 読込・更新・範囲検索

- exactは主キー検索。prefixはBLOBの半開区間`[prefix, bytewise_successor(prefix))`を使い、空prefixは同じroot全体。SQLのLIKEへキーを埋め込まず、`%`・`_`・NUL等をワイルドカードと解釈しない。再帰の`foo`は`foo/`へ解決するので`foo`と`foobar/`を含めない。
- 1ページ256件・512KiB以下とし、次の主キーをopaque cursorにする。深さ・ignoreを同じ有効基準で判定し、除外だけのページでもcursorを進める。全件collect・大きなOFFSETは使わない。
- 観測反映・操作結果反映・削除は短いwrite transactionでcommitし、metadata.revisionを増やす。各objectに反映時revisionを付け、コマンド終了まで未commitにしない。
- MetadataPatchは未照会・既知値・照会したが不明の3状態を区別する。同じ版と確認できる観測では、LISTで未取得の元属性・checksumを保持する。版token・store-modified・サイズが変化した、または同一版と確信できない場合、取得していない旧属性を不明へ戻す。HEAD/GETやコピー結果で得た値は補う。観測した復元状態と版依存の属性を区別する。
- queried_atは実ストアの対象情報を取得した時刻だけで更新し、query・snapshot取得・表示では進めない。

## 同時利用とworker

- 各プロセスにDB専用blocking workerを1つ置き、Tokioから容量16の有界request queueで要求する。書込用1接続、query/snapshot用は必要時に別接続を開き、最大3接続。公開APIは非同期でConnectionをTokio task間へ渡さない。
- WAL、`synchronous=NORMAL`、1接続のpage cache目標8MiB、mmap無効、temp_store=FILEを設定する。これは写し用の耐久性であり、実データのtransaction保証ではない。長時間のreader snapshotを保持しない。
- busy_timeout=2000msでDB内部の短いロック競合だけを待つ。S3や要求操作の再実行とは区別する。超過はエラーを返し、cpならwarning、ls/再照合が完了不能ならerror。アプリ側の無期限再試行をしない。
- 完了結果は判明した順にすみやかに反映要求を出し、プロセス間ではwrite transactionのcommit順で保存する。実ストアの厳密な全順序や時計順を作る永続イベント管理は追加しない。
- DBを開いたままファイルをrename交換せず、WALのmainファイルだけをコピーしない。端末移行は全プロセスを閉じるか下記snapshotを使う。稼働中DBのネットワークFS共有は対象にしない。

## 再照合の範囲照合

P1-07でport/adapterを用意し、P2-05で実ストア更新へ接続する。cpの事前照会を、宛先や指定外の削除照合へ拡張しない。

1. begin_reconcile(selection, ignore)で開始時のDB revisionを取得し、接続専用のTEMP seen表を用意する。FILE-backedでキー集合を全件メモリ保持しない。走査中だけの補助情報であり、履歴・途中再開の状態として残さない。
2. 取得した範囲内かつignore対象外のobjectをseen表へ入れ、逐次upsertする。短いtransactionでcommitし、途中失敗でも取得済み反映を残す。
3. 全範囲の取得が正常終了した場合だけfinishを呼ぶ。正常に照会した空範囲もfinishして成功にできる。一方、全件ignoreで有効対象の状態を確認できなかった場合はerrorとし、finishを呼ばず削除照合へ進まない（D-04/D-06確定）。指定範囲・深さ・有効ignoreに従う既存記録のうち、seenに存在せず、現在のrevisionが開始時revision以下のものを削除候補にする。
4. 候補は主キーでページ取得し、各削除transactionでもrevision条件を確認する。同時操作で更新された行を古い走査で消さず、ignore対象・指定範囲外・未走査深度も消さない。同時走査では保守的に古い記録が残りうるため、次の再照合で再照合する。
5. 列挙失敗・キャンセル時は削除へ入らずTEMP表を破棄する。削除段階自体の途中失敗では完了済み削除と取得済みupsertを残してerror。未処理候補を削除済みとして通知しない。

scopeはSelectionから評価し、呼出し側からSQL断片を受け取らない。正常取得前に削除しないこと、同時upsertを消さないことを検証する。

## load/saveとsnapshot

IndexSnapshotはSQLite Online Backup APIで整合した一時DBを段階的に生成する。1stepは256ページ以下、処理全体にdeadlineを持たせ、writerが多く完了できない場合はエラーにする。DB全体をfull serializeでメモリ展開しない。
完成したsnapshotを閉じ、header・application_id・user_version・PRAGMA quick_checkを確認してからファイルのLoader/Saverへ渡す。importは別ファイルで同じ検査を行う。共有作業DBへの反映・remote競合はP2-02で定め、稼働中DBをそのまま差し替えない。
根拠: [SQLite Online Backup API](https://sqlite.org/backup.html)。

## 未作成・破損と失敗

- 未作成だけはcreate-or-openで空DBを初期化する。同時初期化は一時DBのtransactionと、完全初期化後の同一親へのhard linkによる非上書き公開で扱う。既に公開されたDBがあればそれを開く。既存の空ファイル・不正header・破損・未知の版を未作成と見なさない。
- header/版の検査と実際のqueryで分かった詳細を返し、毎回全DBをintegrity_checkしない。取得snapshotのquick_checkは上記の明示的な取得経路で行う。
- query不能・再照合不能は要求操作のerror。cpの初期化・upsert・commit失敗はwarningとし、実ストア照会結果・manifest・本文転送を独立して使う。同じ故障のwarningを全対象について保持し続けず、当該adapterを無効化した旨を通知できる。
- 自動再構築・空DBへの置換・追加S3走査はしない。保存先変更は切り替えだけ、自動変換なし、破損時は明示的な退避・別の正常/新規保存先選択・指定範囲の再照合とする。未送信分の永続管理・専用復旧は追加しない。S3同期の具体方式はP2-02で決める。

## 実装時の検証

同梱するSQLiteで、2プロセスのquery/update・初期化競合、prefix境界、256件を超えるcursor、NULLと既知値の更新、部分走査失敗、ignore保持、同時upsertの削除回避、最後のobject削除、破損詳細、snapshot再読込を確認する。移行はP2-11で実機とビルド確認を区別する。設計段階のSQL試行を実装検証の代わりとしない。

## P1-07の実装済みAPI

`IndexScope`はroot / recursive / max_depth / ignoreを持ち、syncのSelectionと同じ評価へ変換する。`MetadataPatch` / `FieldPatch`で3状態を表し、exact/query/rootsはIndexRead、反映/削除/範囲照合はIndexWriteで公開する。reconcile sessionのReconcileControlは削除ページ間の取消に使う。sessionの放棄はleaseで検出し、次のworker要求前にTEMPを解放する。snapshotは所有する一時ファイルhandleを返し、受信ファイルの検査は共有DBの交換を行わない。

実装は専用worker1接続、snapshot時も最大2接続（設計上限3以内）、queue16、metadata上限128KiB。未作成時だけ初期化し、破損/未知版を置換しない。9件のRust検証に2プロセス同時利用を含む。S3同期やCLI一覧からの接続は後続タスクで行う。

## 複数条件のquery順序（2026-11-22、D-09a〜c確定）

indexの論理query portにSortSpecを追加し、NameAsc/NameDesc/SizeAsc/SizeDesc/UriAsc/UriDescのenum列を渡す。比較は左から優先し、不明値の位置とURIによる同値の解消を全体要件にそろえる。フロントエンドからSQL断片を渡さない。具象列・ORDER BY・必要な一時索引/ソート領域はDB adapter内部へ閉じる。

CLIの再帰一覧は対象範囲のフラットな結果全体をソートし、GUIは直下の仮想ディレクトリを含む各列をソートする。ページごとだけのソートを全体ソートとして返さない。URI順の既存queryは維持し、任意順のcursorは指定条件・最後の比較値・一意な識別子を含むopaqueな値へ拡張する。同じURIで種別が異なる場合の内部tie-breakはkindで固定する。

SQLite adapterはDB内の順序検索またはFILE-backedの一時ソートを使い、全件Vecを返さない。ソート用の名前は表示と共通の導出規則で求め、キーのdecode・大小文字変換・パス正規化を行わない。サイズ不明の仮想ディレクトリを配下の合計値で埋めない。100万件の複数ソートでも既定のRSS上限を検証する。

## P2-04のquery portとSQLite adapter（2026-11-22）

既存IndexReadを維持し、SortSpec・BrowseRequestとIndexBrowse/IndexListingを追加した。object/children/rootsの投影と完全一致優先、除外通知の有界ページ、取消用BrowseControlを公開する。portにはSQLやS3の保存先情報を含めない。SQLite側で短いsource queryと別の一時結果を組み合わせ、row_numberによる順序番号でページを取得する。SQLのORDER BYはenumからだけ生成する。元のDBのread transactionを表示の寿命まで保持せず、元DBの保存形式/スキーマを変更しない。

元objectsのqueryではnullableなOR条件に依存するページ検索をやめ、namespace/scheme/authorityとBLOB keyの明示的な半開区間・cursor範囲を使う。prefix境界・同時利用・WAL snapshotを含む既存9検証も再確認した。詳細は[Phase 2検証](../../PHASE2_VALIDATION.md)。

## P2-06のファイル取込（2026-11-22）

IndexSnapshotにbegin_importを追加し、SnapshotImport/ImportControl/ImportSummaryを公開した。保存先の取得前にローカルrevisionを捕捉し、検査済みファイルを最大256件/512 KiBのページで取り込む。開始後に更新された行を保持し、不明値とqueried_atを受信時刻で埋め直さない。全ページ取得成功時だけ古い未受信行を短いtransactionで削除する。途中失敗・取消では反映済みページを残し、未受信範囲を削除しない。load(None)は確認済み未作成の写しを表す。IndexDatabaseの論理revisionとcloseも共通portにし、取得/保存の組立ては具体的なDB形式を解釈しない。

SQLite adapterの接続内FILE-backed TEMP表だけを使い、DBのrename交換、永続dirty状態、S3/ETag列は追加しない。import sessionの放棄もleaseで回収する。保存先portはsync、共有組立てManagedIndexとS3のキャッシュ・条件付き転送はinfraに置く。[実装検証](../../PHASE2_VALIDATION.md)を参照。

P2-12の100万件検証では、明示的なprefix下限とcursor下限を併記しても、SQLiteが弱いprefix下限から毎ページ読み直すことを確認した。有効な下限を1つだけ渡し、prefix以上のcursorなら排他的cursor、未指定/下回るcursorなら包含prefixを使う。2万件の先頭/末尾付近の実queryがどちらも6,400 VMステップとなる回帰テストで、既読行の再走査を検出する。DB共通portや保存形式は変えない。

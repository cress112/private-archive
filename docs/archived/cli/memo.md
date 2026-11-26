# private-archive(cli)
### 構想
- 操作interfaceは下記想定
  - 認証はコマンド外で管理
    - デフォルト認証プロファイルを使用
    - 想定としては、profile指定のユーザでセッションプロファイル獲得後、環境変数に値をexportしてからprivate-archiveコマンドを使う
  - 現状はawss3想定
    - 他のデータストアも使用可能になるといいな
    - dddで書きつつ、cli表現種類とindex,syncの具象(infra層)を増やせばいいようにしたい
  - 参照系操作(ls)では、デフォルトでローカルインデックスを参照
  - 更新系操作(cp,sh,rm)では、ローカルインデックスの自動更新も行う
```sh
### cfg: 設定値を確認,更新する
private-archive cfg # 設定可能な項目すべてを表示する
private-archive cfg # TODO: 更新インターフェースを考える, 設定項目を構造的に書き出してoptionの入れ方を考える
### cp: ファイルのコピーを試みる
private-archive cp path/to/file s3://bucket-name/key/of/object # デフォルトデータクラスで配置
private-archive cp path/to/file s3://bucket-name/key/of/object --data-class archived # standard(s3標準), archived(deeparchive)としてcp
private-archive cp s3://bucket-name/key/of/object path/to/file # ローカルへのcp, 即時取得不能(deeparchiveなど)であればエラー終了
### ls: インデックスファイルから条件に合うファイルを探して一覧表示
private-archive ls # 配置箇所(=トップレベル)の一覧表示
private-archive ls s3://bucket-name # 配置箇所で絞り込み
private-archive ls s3://bucket-name/key/of/ # ファイルキーのprefixで絞り込み
private-archive ls s3://bucket-name/key/of/ --update-index # リモートの状態を読んでローカルインデックス更新して表示
private-archive ls s3://bucket-name/key/of/ --detail # データ状態も含めた詳細情報を表示(restore中であればそれも表示)
private-archive ls s3://bucket-name/key/of/ --detail --format json # json listで表示(jqにパイプしてquery可能にしたい)
private-archive ls s3://bucket-name/key/of/ --recursive # {prefix}/**/*を再帰的に表示
private-archive ls s3://bucket-name/key/of/ --recursive --recursive-depth 5 # 5階層下まで再帰的に表示
### ch: ファイルの更新操作を実行する
private-archive ch restore s3://bucket-name/key/of/object # archiveクラスであれば復元要求
private-archive ch restore s3://bucket-name/key/of/ --recursive # 再帰的に復元要求
private-archive ch archive s3://bucket-name/key/of/object # 復元状態であればアーカイブ状態に戻す(復元データを削除)
### rm: ファイルの削除操作を実行する
private-archive rm s3://bucket-name/key/of/object # 確認を出して(terraform applyのように"yes"のみ受付)削除実行
private-archive rm s3://bucket-name/key/of/object --auto-approve # 確認を出さずに削除
private-archive rm s3://bucket-name/key/of/ --recursive # 再帰的に削除
private-archive rm s3://bucket-name/key/of/ --recursive --recursive-depth 2 # 2階層下まで再帰的に削除
```
- ユースケースはこんなかんじ
```sh
### アーカイブデータを確認後、一部リストアしてローカルに落とす
private-archive ls s3://cress-general-data-975049957858-ap-northeast-1-an/private-archive/music --recursive
private-archive ch s3://cress-general-data-975049957858-ap-northeast-1-an/private-archive/music/classic/bach/ --restore --restore-duration 7d --recursive
private-archive ls s3://cress-general-data-975049957858-ap-northeast-1-an/private-archive/music/classic/bach/ --recursive --update-index --format json | jq '[.[] | select(.status == "restoring")]'
private-archive ls s3://cress-general-data-975049957858-ap-northeast-1-an/private-archive/music/classic/bach/ --recursive --update-index --format json | jq '[.[] | select(.status == "restored")]'
private-archive cp s3://cress-general-data-975049957858-ap-northeast-1-an/private-archive/music/classic/bach/ ~/Downloads/bach_archived/ --recursive
private-archive ch s3://cress-general-data-975049957858-ap-northeast-1-an/private-archive/music/classic/bach/ --archive --recursive
```

"""Bounded acceptance-only restore watcher; never part of the application.

One explicit start, hourly HEAD checks, one restore request in acceptance.py.
Results are written locally; no messages, infrastructure APIs, or provisioning.
"""
import argparse
import fcntl
import datetime as dt
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
REPORT = ROOT / "docs/validation/aws-live.json"
REQUIRED = ["three_copy_routes_content", "attributes_listing_reconcile",
            "ordinary_db_local_s3_local", "conditional_304_412_no_overwrite",
            "application_delete", "gui_real_s3_reconcile_and_shared_db",
            "deep_archive_restore_content", "bucket_empty"]
LABELS = ["3方向コピーの内容一致", "属性・一覧・再照合", "通常DBのS3往復",
          "条件付きGET 304／PUT 412・現行本文の保持", "アプリ経由の削除",
          "実S3へのGUI再照合・CLIとのDB共有", "Deep Archive復元後の取得・内容一致",
          "試験後の空バケット"]


def read():
    return json.loads(REPORT.read_text())


def publish():
    d = read()
    complete = d["phase"] == "complete" and all(d["checks"].get(k) for k in REQUIRED)
    status = "完了（全43件完了）" if complete else "未完了：Deep Archive復元待ち" if d["phase"] == "awaiting_restore" else "未完了：" + d["phase"]
    remaining = "試験データは全て清掃し、バケットが空であることを確認した。" if complete else "通常データとインデックスは清掃済みで、待機中に残すのは64KiBのDeep Archive 1個だけ。"
    rows = "\n".join("| " + label + " | " + ("成功" if d["checks"].get(key) else "待機中" if d["phase"] == "awaiting_restore" else "未完了") + " |" for key,label in zip(REQUIRED,LABELS))
    text = f'''# 実AWSでのアプリケーション受入

状態: **{status}**。台帳の段階: `{d['phase']}`。

利用者の指示により、AWS構成の参照・plan/apply・権限追加は実施しない。対象は既存の `cress-archived-data` バケット内に作った専用prefix `{d['prefix']}` だけ。IAMユーザ・ロール・バケットは削除しない。

| 確認 | 結果 |
| --- | --- |
{rows}

[数量台帳と実結果](validation/aws-live.json)を正本とする。CLI/GUI本体は既存の配備バイナリを使用し、Rustの再ビルドは行っていない。{remaining}

## 費用管理

- 許可上限: 追加1 USD。試験計画の予備枠: 0.20 USD。無料枠を前提にしない。
- 独立した上限: S3要求の予約枠400、転送の予約枠20MiB、作成キー4個、本文64KiB、インデックス最大4MiB、復元要求1回（Bulk・1日）。
- 現在の要求予約枠: {d['requests_reserved']}。転送予約枠: {d['transfer_reserved']} bytes。
- 現在の保守的な予約額: {d.get('conservative_reserved_cost_usd',0):.8f} USD。要求を一律0.0001 USD、転送を1 USD/GB、保存・最低180日・追加メタデータ・復元に0.01 USDを確保して計算する。全上限を使っても0.07097152 USD。
- これは実請求額ではない。請求情報の遅延を停止条件にせず、操作前に数量枠を予約し、通常操作を上限368要求で停止して清掃枠32要求を残す。
- 失敗・応答不明の操作も枠を消費する。SDK/AWS CLI/curlの要求再試行は無効。復元要求を繰り返さない。

[単価の根拠](validation/aws-live-pricing.json): [S3 us-east-1公開料金](https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonS3/current/us-east-1/index.json)、[Deep Archive us-east-1公開料金](https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonS3GlacierDeepArchive/current/us-east-1/index.json)。

## 復元待ちと後始末

復元要求時刻: `{d.get('restore_requested_at','未要求')}`。待機期限: `{d.get('restore_deadline','未設定')}`。最新の復元状態: `{json.dumps(d.get('last_restore_status',{}),ensure_ascii=False)}`。

試験専用の `tests/aws/watch.py` が1時間ごとに状態を確認する。アプリには自動監視・更新機能を追加しない。完了後はアプリで取得しSHA256を比較、追跡済みキーだけを削除し、バケットのデータ一覧が空であることを確認する。期限・試験枠・エラーで継続できない場合は未完了として記録し、追跡済み試験データを清掃する。

監視ログ: `target/aws-live-watch.log`。監視はこのMacが動作し、AWS認証・通信が利用できる間に進む。手動で再開する場合は、監視の状態を台帳で確認してから次を使う（ローカルロックで重複実行を防ぐ）。

```sh
/bin/bash scripts/private-archive-session.sh python3 tests/aws/acceptance.py check
```

## 検証スクリプトの修正記録

初回の304判定でAWS CLIの終了コードを255と誤認したが、実際は254で、S3の304応答自体は成功していた。また、再開用メソッド名とPath属性の衝突を修正した。どちらも台帳へ記録し、停止時に専用キーを清掃した。成功済みの3方向コピーやDB往復を再試験せず、予約枠を保持したまま未完了項目だけを継続した。インフラやアプリの不具合として扱っていない。

AWS CLI 2.18.3はPutObjectのIf-Match引数に未対応のため、この1要求だけcurl 8.7.1のSigV4を使い、資格情報は標準入力から渡した。現行ETagが変化したことを確認してから古いETagを送り、412と現行ETag不変を確認した。製品自身の412通知・再送しない経路は従来のHTTP/GUI試験を根拠とする。

画像: [実S3から得た記録の詳細](validation/aws-live-gui-file.png)、[削除済み範囲へのGUI再照合](validation/aws-live-gui-empty.png)。
'''
    (ROOT / "docs/AWS_VALIDATION.md").write_text(text)
    for name,target in [("README.md","docs/AWS_VALIDATION.md"),("docs/FINAL_ACCEPTANCE.md","AWS_VALIDATION.md")]:
        p = ROOT / name
        content = p.read_text()
        replacement = "<!-- aws-acceptance:start -->\n実AWS受入: **" + status + "**。[検証結果と費用台帳](" + target + ")。\n<!-- aws-acceptance:end -->"
        if "<!-- aws-acceptance:start -->" not in content:
            raise RuntimeError("missing status marker in " + name)
        updated = re.sub(r"<!-- aws-acceptance:start -->.*?<!-- aws-acceptance:end -->",lambda _:replacement,content,flags=re.S)
        if updated != content:
            p.write_text(updated)
    print(json.dumps({"time":dt.datetime.now(dt.timezone.utc).isoformat(),"phase":d["phase"],"complete":complete,"requests_reserved":d["requests_reserved"],"reserved_cost_usd":d.get("conservative_reserved_cost_usd")}),flush=True)
    return d


def monitor_state(status, due=None):
    path = ROOT / "target/aws-live-monitor.json"
    data = {"pid":os.getpid(), "status":status,
            "updated":dt.datetime.now(dt.timezone.utc).isoformat(),
            "next_check":dt.datetime.fromtimestamp(due,dt.timezone.utc).isoformat() if due else None}
    tmp = path.with_suffix(".tmp"); tmp.write_text(json.dumps(data,indent=2)+"\n");tmp.replace(path)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--publish-only", action="store_true")
    args = parser.parse_args()
    d = publish()
    if args.publish_only or d["phase"] != "awaiting_restore":
        return
    monitor_lock = (ROOT/"target/aws-live-watch.lock").open("a")
    fcntl.flock(monitor_lock,fcntl.LOCK_EX | fcntl.LOCK_NB)
    deadline = dt.datetime.fromisoformat(d["restore_deadline"]).timestamp()
    due = min(time.time()+3600, deadline)
    for _ in range(74):
        monitor_state("waiting",due)
        while time.time() < due:
            time.sleep(max(0, min(60, due-time.time())))
        d = read()
        if d["phase"] != "awaiting_restore":
            publish(); monitor_state("stopped"); return
        monitor_state("checking")
        command = ["/bin/bash",str(ROOT/"scripts/private-archive-session.sh"),sys.executable,
                   str(ROOT/"tests/aws/acceptance.py"),"check"]
        env = dict(os.environ,PRIVATE_ARCHIVE_AWS_PROFILE="private-archive",AWS_MAX_ATTEMPTS="1",AWS_RETRY_MODE="standard")
        try:
            result = subprocess.run(command,cwd=ROOT,env=env,capture_output=True,text=True,timeout=660)
        except subprocess.TimeoutExpired:
            print("acceptance process exceeded timeout; inspect ledger before further operations",flush=True)
            publish(); monitor_state("stopped"); return
        print(result.stdout,flush=True)
        if result.stderr: print(result.stderr,flush=True)
        d = publish()
        if result.returncode != 0 or d["phase"] != "awaiting_restore":
            monitor_state("complete" if d["phase"] == "complete" else "stopped")
            return
        due = min(time.time()+3600,deadline)
    raise RuntimeError("monitor check limit reached; inspect ledger")


if __name__ == "__main__":
    main()

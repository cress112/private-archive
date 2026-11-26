# /// script
# requires-python = ">=3.10"
# dependencies = ["boto3==1.44.3"]
# ///
"""Product CLI acceptance against the isolated local fixture. Never uses AWS auth."""
import argparse, collections, datetime, platform, re, hashlib, http.client, http.server, json, os
import pathlib, random, resource, shutil, signal, subprocess, sys, tempfile, threading, time, urllib.parse, uuid
import boto3
from botocore.config import Config

ROOT=pathlib.Path(__file__).resolve().parents[2]
BUCKET="private-archive-test"
ADMIN=boto3.client("s3",endpoint_url="http://127.0.0.1:18333",region_name="us-east-1",aws_access_key_id="private-archive-test",aws_secret_access_key="private-archive-test-secret",config=Config(retries={"total_max_attempts":1},connect_timeout=2,read_timeout=120,s3={"addressing_style":"path"}))
class Metrics:
    def __init__(self):self.lock=threading.Lock();self.records=[];self.active=0;self.peak=0
    def begin(self):
        with self.lock:self.active+=1;self.peak=max(self.peak,self.active)
    def end(self,record):
        with self.lock:self.records.append(record);self.active-=1
METRICS=Metrics()
class Proxy(http.server.BaseHTTPRequestHandler):
    protocol_version="HTTP/1.1"
    def log_message(self,*args):pass
    def route(self):
        METRICS.begin();query=urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query,keep_blank_values=True)
        copied=self.headers.get("x-amz-copy-source") is not None
        action=("UploadPartCopy" if copied else "UploadPart") if "partNumber" in query else "Create" if "uploads" in query else "Complete" if self.command=="POST" and "uploadId" in query else "Abort" if self.command=="DELETE" and "uploadId" in query else "CopyObject" if copied else self.command
        record={"action":action,"part":query.get("partNumber",[None])[0],"range":self.headers.get("x-amz-copy-source-range"),"request_bytes":0,"response_bytes":0,"status":None}
        connection=http.client.HTTPConnection("127.0.0.1",18333,timeout=900)
        try:
            connection.putrequest(self.command,self.path,skip_host=True,skip_accept_encoding=True)
            for name,value in self.headers.items():
                if name.lower() not in ("connection","expect"):connection.putheader(name,value)
            connection.putheader("Connection","close");connection.endheaders()
            if self.headers.get("Transfer-Encoding","").lower()=="chunked":
                while True:
                    line=self.rfile.readline();connection.send(line);size=int(line.split(b";",1)[0],16)
                    if size==0:
                        while True:
                            line=self.rfile.readline();connection.send(line)
                            if line==b"\r\n":break
                        break
                    remaining=size+2
                    while remaining:
                        block=self.rfile.read(min(65536,remaining));connection.send(block);remaining-=len(block)
                    record["request_bytes"]+=size
            else:
                remaining=int(self.headers.get("Content-Length","0"))
                while remaining:
                    block=self.rfile.read(min(65536,remaining))
                    if not block:raise EOFError("short CLI request")
                    connection.send(block);remaining-=len(block);record["request_bytes"]+=len(block)
            response=connection.getresponse();record["status"]=response.status;self.send_response(response.status)
            for name,value in response.getheaders():
                if name.lower() not in ("connection","transfer-encoding"):self.send_header(name,value)
            self.send_header("Connection","close");self.end_headers()
            if self.command!="HEAD":
                while block:=response.read(65536):self.wfile.write(block);record["response_bytes"]+=len(block)
            self.wfile.flush()
        finally:connection.close();self.close_connection=True;METRICS.end(record)
    do_GET=do_HEAD=do_PUT=do_POST=do_DELETE=route

def digest(path):
    h=hashlib.sha256()
    with open(path,"rb")as source:
        while block:=source.read(1024*1024):h.update(block)
    return h.hexdigest()
def cli(binary,endpoint,work,args,expected=0):
    env=dict(os.environ)
    for name in list(env):
        if name.startswith("AWS_"):del env[name]
    env["HOME"]=str(work/"home")
    command=[str(binary),"--test-endpoint",endpoint,"--config",str(work/"config.toml"),"--index-location",str(work/"index.db"),*args]
    before=len(METRICS.records);start=time.monotonic();out=work/"stdout";err=work/"stderr"
    with open(out,"wb")as stdout,open(err,"wb")as stderr:
        process=subprocess.Popen(command,stdout=stdout,stderr=stderr,stdin=subprocess.DEVNULL,env=env,cwd=work)
        _,status,usage=os.wait4(process.pid,0);process.returncode=os.waitstatus_to_exitcode(status)
    elapsed=time.monotonic()-start
    if process.returncode!=expected:raise AssertionError(f"CLI exit {process.returncode}, expected {expected}: {err.read_text()}")
    while METRICS.active:time.sleep(.01)
    records=METRICS.records[before:];rss=usage.ru_maxrss if sys.platform=="darwin" else usage.ru_maxrss*1024
    result={"exit":process.returncode,"seconds":round(elapsed,3),"rss_bytes":rss,"requests":dict(collections.Counter(r["action"]for r in records)),"parts":[{"number":r["part"],"range":r["range"],"request_bytes":r["request_bytes"]}for r in records if r["part"]],"wire_upload_bytes":sum(r["request_bytes"]for r in records if r["action"]in("PUT","UploadPart")),"wire_download_bytes":sum(r["response_bytes"]for r in records if r["action"]=="GET")}
    if expected==0:assert "error:" not in err.read_text();assert not any("warning" in line for line in out.read_text().splitlines())
    print(json.dumps({"cli": args[0], "seconds":result["seconds"],"rss_mib":round(rss/1024**2,2),"requests":result["requests"]}),flush=True)
    return result

class Monitor:
    def __init__(self,work):self.work=work;self.stop=threading.Event();self.low=threading.Event();self.server_rss=0;self.local_peak=0;self.min_free=shutil.disk_usage(ROOT).free;self.thread=threading.Thread(target=self.loop,daemon=True)
    def loop(self):
        while not self.stop.is_set():
            self.min_free=min(self.min_free,shutil.disk_usage(ROOT).free)
            if self.min_free<8*1024**3:self.low.set()
            local=0
            for path in self.work.rglob("*"):
                try:
                    if path.is_file():local+=path.stat().st_blocks*512
                except FileNotFoundError:pass
            self.local_peak=max(self.local_peak,local)
            try:
                status=subprocess.run(["docker","exec","private-archive-s3-tests-s3-1","cat","/proc/1/status"],capture_output=True,text=True,timeout=5,check=True).stdout
                self.server_rss=max(self.server_rss,int(next(line.split()[1]for line in status.splitlines()if line.startswith("VmRSS:")))*1024)
            except (subprocess.SubprocessError,StopIteration,ValueError):pass
            self.stop.wait(1)
    def __enter__(self):self.thread.start();return self
    def __exit__(self,*args):self.stop.set();self.thread.join()
def vm_disk():
    output=subprocess.run(["docker","exec","private-archive-s3-tests-s3-1","df","-k","/data"],capture_output=True,text=True,timeout=10,check=True).stdout.splitlines()[-1].split()
    return {"used_bytes":int(output[2])*1024,"free_bytes":int(output[3])*1024}

def run_size(binary,endpoint,work,mib,large):
    size=mib*1024*1024;prefix=f"cli-accept-{uuid.uuid4().hex}/";a=prefix+"a";b=prefix+"b";source=work/"source";download=work/"download";results={"mib":mib,"bytes":size,"sha256":None,"operations":{},"disk_before":shutil.disk_usage(ROOT).free}
    # The input is deleted after upload, and downloads are removed after hash
    # verification. No two local payload files are retained at the same time.
    if large and results["disk_before"]<size*3+8*1024**3:raise RuntimeError(f"insufficient host space for {mib}MiB with an 8GiB reserve: {results['disk_before']}")
    results["vm_disk_before"]=vm_disk()
    rng=random.Random(20261116);hash=hashlib.sha256()
    with open(source,"wb")as file:
        for _ in range(mib):
            block=rng.randbytes(1024*1024);file.write(block);hash.update(block)
    os.chmod(source,0o640);os.utime(source,ns=(1_300_000_000_000000123,1_300_000_000_000000123));mtime=source.stat().st_mtime_ns;results["sha256"]=hash.hexdigest()
    try:
        print(f"START {mib}MiB upload",flush=True);results["operations"]["upload"]=cli(binary,endpoint,work,["cp",str(source),f"s3://{BUCKET}/{a}"]);source.unlink()
        results["operations"]["download_a"]=cli(binary,endpoint,work,["cp",f"s3://{BUCKET}/{a}",str(download)])
        assert digest(download)==results["sha256"];assert download.stat().st_mtime_ns==mtime;assert download.stat().st_mode&0o7777==0o640;download.unlink()
        results["operations"]["copy"]=cli(binary,endpoint,work,["cp",f"s3://{BUCKET}/{a}",f"s3://{BUCKET}/{b}"])
        ADMIN.delete_object(Bucket=BUCKET,Key=a)
        results["operations"]["download_b"]=cli(binary,endpoint,work,["cp",f"s3://{BUCKET}/{b}",str(download)])
        assert digest(download)==results["sha256"];assert download.stat().st_mtime_ns==mtime;assert download.stat().st_mode&0o7777==0o640;download.unlink()
        upload_parts=0 if size<=16*1024*1024 else (size+16*1024*1024-1)//(16*1024*1024);copy_parts=0 if size<=16*1024*1024 else(size+128*1024*1024-1)//(128*1024*1024)
        assert results["operations"]["upload"]["requests"].get("UploadPart",0)==upload_parts
        assert results["operations"]["copy"]["requests"].get("UploadPartCopy",0)==copy_parts
        uploaded=results["operations"]["upload"]["parts"];copied=results["operations"]["copy"]["parts"]
        assert sorted(int(part["number"])for part in uploaded)==list(range(1,upload_parts+1))
        assert sorted(int(part["number"])for part in copied)==list(range(1,copy_parts+1))
        for part in uploaded:
            offset=(int(part["number"])-1)*16*1024*1024;assert part["request_bytes"]==min(16*1024*1024,size-offset)
        for part in copied:
            offset=(int(part["number"])-1)*128*1024*1024;assert part["range"]==f"bytes={offset}-{min(offset+128*1024*1024,size)-1}"
        assert results["operations"]["upload"]["wire_upload_bytes"]==size
        for name in ["download_a","download_b"]:assert results["operations"][name]["wire_download_bytes"]==size
        for value in results["operations"].values():assert value["rss_bytes"]<=512*1024*1024,value
        results["upload_parts"]=upload_parts;results["copy_parts"]=copy_parts
    finally:
        for key in [a,b]:ADMIN.delete_object(Bucket=BUCKET,Key=key)
        for page in ADMIN.get_paginator("list_multipart_uploads").paginate(Bucket=BUCKET,Prefix=prefix):
            for item in page.get("Uploads",[]):ADMIN.abort_multipart_upload(Bucket=BUCKET,Key=item["Key"],UploadId=item["UploadId"])
        for path in [source,download]:path.unlink(missing_ok=True)
    assert not ADMIN.list_objects_v2(Bucket=BUCKET,Prefix=prefix).get("Contents")
    assert not ADMIN.list_multipart_uploads(Bucket=BUCKET,Prefix=prefix).get("Uploads")
    results["vm_disk_after"]=vm_disk()
    results["disk_after"]=shutil.disk_usage(ROOT).free;print(json.dumps(results),flush=True);return results

def functional(binary,endpoint):
    prefix=f"cli-tree-{uuid.uuid4().hex}/"
    with tempfile.TemporaryDirectory(prefix="private-archive-tree-")as directory:
        work=pathlib.Path(directory);tree=work/"tree";tree.mkdir();(tree/"sub").mkdir();(tree/"a").write_bytes(b"a");(tree/"b.tmp").write_bytes(b"ignored");(tree/"sub/c").write_bytes(b"deep");(tree/"ignored.link").symlink_to(tree/"a")
        (work/"config.toml").write_text('ignore=["*.tmp","ignored.link"]\n')
        try:
            result=cli(binary,endpoint,work,["cp",str(tree)+"/",f"s3://{BUCKET}/{prefix}","--recursive","--recursive-depth","1"])
            assert result["requests"].get("PUT")==1
            keys=[item["Key"]for page in ADMIN.get_paginator("list_objects_v2").paginate(Bucket=BUCKET,Prefix=prefix)for item in page.get("Contents",[])]
            assert keys==[prefix+"a"],keys
            (tree/"a").write_bytes(b"replacement");cli(binary,endpoint,work,["cp",str(tree/"a"),f"s3://{BUCKET}/{prefix}a"])
            ADMIN.put_object(Bucket=BUCKET,Key=prefix+"b.tmp",Body=b"external")
            output=work/"out";cli(binary,endpoint,work,["cp",f"s3://{BUCKET}/{prefix}",str(output),"--recursive","--ignore",""])
            assert (output/"a").read_bytes()==b"replacement";assert (output/"b.tmp").read_bytes()==b"external"
            result=cli(binary,endpoint,work,["cp",f"s3://{BUCKET}/{prefix}a",str(work/"skip.tmp"),"--ignore","*.tmp"],expected=1)
            assert result["requests"].get("GET",0)==0;assert not (work/"skip.tmp").exists();assert (work/"stdout").read_text()==""
            result=cli(binary,endpoint,work,["cp",str(tree/"ignored.link"),f"s3://{BUCKET}/{prefix}link"],expected=1)
            assert result["requests"]=={} and (work/"stdout").read_text()==""
            (work/"config.toml").write_text("");result=cli(binary,endpoint,work,["cp",str(tree/"ignored.link"),f"s3://{BUCKET}/{prefix}link"],expected=1);assert result["requests"].get("PUT",0)==0
            os.mkfifo(tree/"fifo");result=cli(binary,endpoint,work,["cp",str(tree/"fifo"),f"s3://{BUCKET}/{prefix}fifo"],expected=1);assert result["requests"].get("PUT",0)==0
        finally:
            for page in ADMIN.get_paginator("list_objects_v2").paginate(Bucket=BUCKET,Prefix=prefix):
                for item in page.get("Contents",[]):ADMIN.delete_object(Bucket=BUCKET,Key=item["Key"])
    return ["recursive trailing slash layout","recursive depth 1","configured ignore","runtime empty ignore override","local and S3 overwrite","destination ignore without GET","ignored symlink","symlink/FIFO rejection before body"]

def reset_fixture():
    assert not ADMIN.list_objects_v2(Bucket=BUCKET,MaxKeys=1).get("Contents"),"fixture contains live objects; refusing reset"
    assert not ADMIN.list_multipart_uploads(Bucket=BUCKET,MaxUploads=1).get("Uploads"),"fixture contains live uploads; refusing reset"
    compose=["docker","compose","-f",str(ROOT/"tests/s3/compose.yaml")]
    subprocess.run([*compose,"down","--volumes"],check=True,capture_output=True)
    subprocess.run([*compose,"up","-d"],check=True,capture_output=True)
    for attempt in range(60):
        try:ADMIN.head_bucket(Bucket=BUCKET);break
        except Exception:
            if attempt==59:raise
            time.sleep(1)
    print("fixture reset: empty dedicated volume recreated",flush=True)

def main():
    parser=argparse.ArgumentParser();parser.add_argument("--binary",type=pathlib.Path,default=ROOT/"target/debug/private-archive");parser.add_argument("--sizes-mib",type=int,nargs="+",default=[1]);parser.add_argument("--report",type=pathlib.Path,required=True);parser.add_argument("--large",action="store_true");parser.add_argument("--functional",action="store_true");parser.add_argument("--reset-fixture-between-sizes",action="store_true");args=parser.parse_args()
    server=http.server.ThreadingHTTPServer(("127.0.0.1",0),Proxy);thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start();endpoint=f"http://127.0.0.1:{server.server_port}";lock=(ROOT/"Cargo.lock").read_text()
    versions={}
    for package in lock.split("[[package]]"):
        name=re.search(r'^name = "([^"]+)"',package,re.M);version=re.search(r'^version = "([^"]+)"',package,re.M)
        if name and version and name[1]in["aws-sdk-s3","aws-config","rusqlite","libsqlite3-sys","tokio"]:versions[name[1]]=version[1]
    tree=hashlib.sha256()
    for path in sorted([ROOT/"Cargo.toml",*ROOT.glob("crates/*/Cargo.toml"),*ROOT.glob("crates/**/*.rs")]):tree.update(str(path.relative_to(ROOT)).encode());tree.update(path.read_bytes())
    image=subprocess.run(["docker","inspect","--format","{{.Config.Image}}","private-archive-s3-tests-s3-1"],capture_output=True,text=True,check=True).stdout.strip()
    report={"payload_generator":"Random(20261116), advancing 1MiB blocks, non-sparse writes","started_utc":datetime.datetime.now(datetime.timezone.utc).isoformat(),"os":platform.platform(),"cpu":platform.machine(),"rustc":subprocess.run(["rustc","--version"],capture_output=True,text=True,check=True).stdout.strip(),"software":versions,"bundled_sqlite_rust_validation":"3.54.1","image":image,"endpoint":endpoint,"namespace":"test:"+endpoint,"binary_sha256":digest(args.binary.resolve()),"cargo_lock_sha256":digest(ROOT/"Cargo.lock"),"rust_source_sha256":tree.hexdigest(),"cases":[]}
    try:
        if args.functional:report["functional"]=functional(args.binary.resolve(),endpoint)
        for mib in args.sizes_mib:
            if args.reset_fixture_between_sizes:reset_fixture()
            with tempfile.TemporaryDirectory(prefix="private-archive-cli-")as directory:
                work=pathlib.Path(directory);(work/"config.toml").write_text('[copy]\ndata-class="standard"\n')
                with Monitor(work) as monitor:result=run_size(args.binary.resolve(),endpoint,work,mib,args.large)
                result.update(server_peak_rss_bytes=monitor.server_rss,local_peak_allocated_bytes=monitor.local_peak,host_min_free_bytes=monitor.min_free)
                if args.large:assert not monitor.low.is_set(),"host reserve dropped below 8GiB"
                report["cases"].append(result);args.report.write_text(json.dumps(report,indent=2)+"\n")
        by_size={c["mib"]:c for c in report["cases"]}
        if 1024 in by_size and 4096 in by_size:
            for route in ["upload","download_a","copy","download_b"]:assert by_size[4096]["operations"][route]["rss_bytes"]-by_size[1024]["operations"][route]["rss_bytes"]<=64*1024*1024
        assert METRICS.peak<=4
        report["finished_utc"]=datetime.datetime.now(datetime.timezone.utc).isoformat()
        report["peak_proxy_requests"]=METRICS.peak;args.report.write_text(json.dumps(report,indent=2)+"\n")
    finally:server.shutdown();server.server_close();thread.join()
if __name__=="__main__":main()

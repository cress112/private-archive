# /// script
# requires-python = ">=3.10"
# ///
"""Fault/interrupt tests of the real fixture CLI. Uses only loopback HTTP."""
import argparse,contextlib,http.server,json,os,pathlib,signal,socket,sqlite3,subprocess,tempfile,threading,time,urllib.parse
ROOT=pathlib.Path(__file__).resolve().parents[2]
class Server(http.server.ThreadingHTTPServer):daemon_threads=True;block_on_close=False
class State:
    def __init__(self,route):self.route=route;self.seen=[];self.lock=threading.Lock();self.part=threading.Event();self.release=threading.Event()
class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version="HTTP/1.1"
    def log_message(self,*args):pass
    def handle_request(self):
        length=int(self.headers.get("Content-Length","0"));remaining=length
        while remaining:
            block=self.rfile.read(min(65536,remaining))
            if not block:break
            remaining-=len(block)
        request={"method":self.command,"path":self.path,"length":length,"copy":self.headers.get("x-amz-copy-source") is not None}
        with self.server.state.lock:self.server.state.seen.append(request)
        status,headers,body,hang=self.server.state.route(request,self.server.state)
        if hang:self.server.state.part.set();self.server.state.release.wait(60)
        if status==0:
            self.close_connection=True;self.connection.shutdown(socket.SHUT_RDWR);return
        try:
            self.send_response(status);self.send_header("Content-Length",str(headers.pop("Content-Length",len(body))));self.send_header("x-amz-request-id","fault-request")
            for name,value in headers.items():self.send_header(name,value)
            self.send_header("Connection","close");self.end_headers()
            if self.command!="HEAD":self.wfile.write(body)
            self.wfile.flush()
        except (BrokenPipeError,ConnectionResetError):pass
        self.close_connection=True
    do_HEAD=do_GET=do_PUT=do_POST=do_DELETE=handle_request
@contextlib.contextmanager
def fixture(route,handler=Handler):
    state=State(route);server=Server(("127.0.0.1",0),handler);server.state=state;thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
    try:yield state,f"http://127.0.0.1:{server.server_port}"
    finally:state.release.set();server.shutdown();server.server_close();thread.join()
def response(status=200,body=b"",headers=None,hang=False):return status,dict(headers or {}),body,hang
def error(status=500):return response(status,b"<Error><Code>InternalError</Code><Message>injected failure</Message></Error>")
def head(size=0,**headers):return response(headers={"Content-Length":size,**headers})
def request_kind(request):
    query=urllib.parse.parse_qs(urllib.parse.urlsplit(request["path"]).query,keep_blank_values=True)
    return "list" if "list-type"in query else "create"if"uploads"in query else"abort"if request["method"]=="DELETE"else"complete"if request["method"]=="POST"else request["method"].lower()
def command(binary,endpoint,work,args):return[str(binary),"--test-endpoint",endpoint,"--config",str(work/"config"),"--index-location",str(work/"index.db"),*args]
def environment(work):
    env={k:v for k,v in os.environ.items()if not k.startswith("AWS_")};env["HOME"]=str(work/"home");return env
def run(binary,endpoint,work,args,expected):
    result=subprocess.run(command(binary,endpoint,work,args),cwd=work,env=environment(work),capture_output=True,timeout=58)
    assert result.returncode==expected,(result.returncode,result.stderr.decode());assert b"warning"not in result.stdout;return result

def main():
    parser=argparse.ArgumentParser();parser.add_argument("--binary",type=pathlib.Path,default=ROOT/"target/debug/private-archive");parser.add_argument("--report",type=pathlib.Path,required=True);args=parser.parse_args();binary=args.binary.resolve();passed=[]
    with tempfile.TemporaryDirectory(prefix="private-archive-fault-")as directory:
        work=pathlib.Path(directory);(work/"config").write_text("");out=work/"output"
        for status in [403,404,500]:
            with fixture(lambda request,state:response(status))as(state,endpoint):
                result=run(binary,endpoint,work,["cp","s3://bucket/source",str(out)],1);assert len(state.seen)==1 and state.seen[0]["method"]=="HEAD";assert b"fault-request"in result.stderr;assert result.stdout==b"";passed.append(f"HEAD {status}: one request, no transfer")
        def late_list(request,state):
            kind=request_kind(request)
            if kind=="list":
                if "continuation-token"in request["path"]:return error()
                return response(body=b"<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>later</NextContinuationToken><Contents><Key>src/a</Key><Size>1</Size></Contents></ListBucketResult>")
            return head(1)if"/src/"in request["path"]else response(404)
        with fixture(late_list)as(state,endpoint):
            result=run(binary,endpoint,work,["cp","s3://bucket/src","s3://bucket/dest","--recursive"],1);assert not any(request_kind(r)=="put"or(request_kind(r)=="get")for r in state.seen);assert result.stdout==b"";passed.append("late LIST failure: zero body/copy requests")
        for corrupt in [False,True]:
            out.write_bytes(b"original")
            def broken(request,state):
                if request["method"]=="HEAD":return head(3)
                return response(body=b"abc"if corrupt else b"ab",headers={"Content-Length":3,"x-amz-checksum-type":"FULL_OBJECT","x-amz-checksum-crc32":"AAAAAA=="})
            with fixture(broken)as(state,endpoint):
                result=run(binary,endpoint,work,["cp","s3://bucket/source",str(out)],1);assert out.read_bytes()==b"original";assert result.stdout==b"";assert not list(work.glob(".private-archive-*"));passed.append("bad checksum preserves destination"if corrupt else"short GET preserves destination")
        def copy_route(request,state):
            if request["method"]=="HEAD":return head()if"/source"in request["path"]or any(r["method"]=="PUT"for r in state.seen)else response(404)
            return response(body=b'<CopyObjectResult><ETag>"copy"</ETag></CopyObjectResult>')
        with fixture(lambda request,state: response(200,b"<Error><Code>InternalError</Code><Message>embedded</Message></Error>")if request["method"]=="PUT"else copy_route(request,state))as(state,endpoint):
            result=run(binary,endpoint,work,["cp","s3://bucket/source","s3://bucket/dest"],1);assert sum(r["method"]=="PUT"for r in state.seen)==1;assert result.stdout==b"";passed.append("Copy HTTP 200 Error: failure without retry")
        with fixture(lambda request,state:response(0)if request["method"]=="PUT"else copy_route(request,state))as(state,endpoint):
            result=run(binary,endpoint,work,["cp","s3://bucket/source","s3://bucket/dest"],1)
            assert b"outcome=Unknown"in result.stderr;assert result.stdout==b"";assert sum(r["method"]=="PUT"for r in state.seen)==1
            passed.append("lost mutation response: Unknown, exit1, no success or retry")
        (work/"index.db").write_bytes(b"broken index")
        with fixture(copy_route)as(state,endpoint):
            result=run(binary,endpoint,work,["cp","s3://bucket/source","s3://bucket/dest"],0);assert b"[Index]"in result.stderr;assert (work/"index.db").read_bytes()==b"broken index";assert result.stdout.startswith(b"copy ");passed.append("corrupt index warning with successful copy, corrupt bytes preserved")
        (work/"index.db").unlink()
        def unknown_attributes(request,state):
            headers={"x-amz-meta-private-archive-version":"1","x-amz-meta-private-archive-mtime":"bad","x-amz-meta-private-archive-mode":"9999"}
            if request["method"]=="HEAD":return head(1,**headers)
            return response(body=b"x",headers=headers)
        with fixture(unknown_attributes)as(state,endpoint):
            result=run(binary,endpoint,work,["cp","s3://bucket/source",str(out)],0);assert out.read_bytes()==b"x";assert b"[Attributes]"in result.stderr;passed.append("malformed/missing attributes warn; body succeeds")
        partial=work/"partial";partial.mkdir();(partial/"b").write_bytes(b"existing b")
        def partial_route(request,state):
            kind=request_kind(request)
            if kind=="list":return response(body=b"<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>src/a</Key><Size>1</Size></Contents><Contents><Key>src/b</Key><Size>3</Size></Contents><Contents><Key>src/c</Key><Size>1</Size></Contents></ListBucketResult>")
            if kind=="head":return head(3 if "/src/b"in request["path"]else 1)
            if "/src/b"in request["path"]:return response(body=b"ab",headers={"Content-Length":3})
            return response(body=b"a")
        with fixture(partial_route)as(state,endpoint):
            result=run(binary,endpoint,work,["cp","s3://bucket/src",str(partial),"--recursive"],1)
            assert (partial/"a").read_bytes()==b"a";assert (partial/"b").read_bytes()==b"existing b";assert not (partial/"c").exists()
            assert len(result.stdout.splitlines())==1;assert not any(r["method"]=="GET"and"/src/c"in r["path"]for r in state.seen)
            passed.append("first transfer failure stops next file; prior success output and existing failed destination preserved")
        source=work/"source"
        with source.open("wb")as file:
            for _ in range(17):file.write(b"x"*(1024*1024))
        def upload_route(request,state):
            kind=request_kind(request)
            if kind=="head":return response(404)
            if kind=="create":return response(body=b"<InitiateMultipartUploadResult><UploadId>own-upload-only</UploadId></InitiateMultipartUploadResult>")
            assert"uploadId=own-upload-only"in request["path"],request
            return error()
        with fixture(upload_route)as(state,endpoint):
            result=run(binary,endpoint,work,["cp",str(source),"s3://bucket/dest"],1);assert sum(request_kind(r)=="abort"for r in state.seen)==1;assert not any(request_kind(r)=="complete"for r in state.seen);assert b"[Cleanup]"in result.stderr;assert result.stdout==b"";passed.append("part failure stops; Abort own ID once; Abort failure warning preserves original failure")
        def stalled(request,state):
            kind=request_kind(request)
            if kind=="head":return response(404)
            if kind=="create":return response(body=b"<InitiateMultipartUploadResult><UploadId>own-stalled-upload</UploadId></InitiateMultipartUploadResult>")
            assert"uploadId=own-stalled-upload"in request["path"]
            if kind=="abort":return response(204,hang=True)
            return response(headers={"ETag":'"part"'},hang=True)
        with fixture(stalled)as(state,endpoint):
            process=subprocess.Popen(command(binary,endpoint,work,["cp",str(source),"s3://bucket/dest"]),cwd=work,env=environment(work),stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            assert state.part.wait(15);start=time.monotonic();process.send_signal(signal.SIGINT);stdout,stderr=process.communicate(timeout=52);elapsed=time.monotonic()-start;assert process.returncode==130,(process.returncode,stderr.decode());assert elapsed<=50;assert not stdout;assert sum(request_kind(r)=="abort"for r in state.seen)==1;assert not any(request_kind(r)=="complete"for r in state.seen);passed.append(f"SIGINT with unresponsive parts and Abort: exit130 in {elapsed:.3f}s, own Abort once")
        small=work/"small";small.write_bytes(b"x")
        def unresponsive_put(request,state):return response(404)if request["method"]=="HEAD"else response(hang=True)
        with fixture(unresponsive_put)as(state,endpoint):
            process=subprocess.Popen(command(binary,endpoint,work,["cp",str(small),"s3://bucket/dest"]),cwd=work,env=environment(work),stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            assert state.part.wait(15);start=time.monotonic();process.send_signal(signal.SIGINT);stdout,stderr=process.communicate(timeout=35);elapsed=time.monotonic()-start
            assert process.returncode==130,(process.returncode,stderr.decode());assert b"outcome=Unknown"in stderr;assert not stdout;assert sum(r["method"]=="PUT"for r in state.seen)==1
            passed.append(f"SIGINT during single PUT with lost response: Unknown + exit130 in {elapsed:.3f}s, no retry")
        small.unlink()
        source.unlink()
        with fixture(copy_route)as(state,endpoint):
            process=subprocess.Popen(command(binary,endpoint,work,["cp","s3://bucket/source","s3://bucket/dest"]),cwd=work,env=environment(work),stdout=subprocess.PIPE,stderr=subprocess.PIPE);process.stdout.close();stderr=process.stderr.read();code=process.wait(timeout=10);assert code==1,(code,stderr);assert sum(r["method"]=="PUT"for r in state.seen)==1;assert sqlite3.connect(work/"index.db").execute("SELECT count(*) FROM objects WHERE object_key=?",(b"dest",)).fetchone()[0]==1;passed.append("broken stdout errors after confirmed copy; destination index record retained")
    args.report.write_text(json.dumps({"passed":passed,"count":len(passed)},indent=2)+"\n");print(json.dumps({"passed":passed,"count":len(passed)}),flush=True)
if __name__=="__main__":main()

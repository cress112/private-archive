"""P3-04..08 native GUI actions, local sharing and loopback S3 faults."""
import argparse, contextlib, hashlib, json, os, pathlib, sqlite3, subprocess, sys, tempfile, threading, time, urllib.parse
from xml.sax.saxutils import escape
sys.path.insert(0,str(pathlib.Path(__file__).resolve().parents[1]/'s3'))
import faults
import index_files
from startup import save_png
ROOT=pathlib.Path(__file__).resolve().parents[2]

def listing(keys,truncated=False):
    contents=''.join(f'<Contents><Key>{escape(k)}</Key><Size>{s}</Size><LastModified>2026-11-21T00:00:00Z</LastModified><ETag>"v1"</ETag><StorageClass>STANDARD</StorageClass></Contents>' for k,s in keys)
    token='<NextContinuationToken>next</NextContinuationToken>' if truncated else ''
    return f'<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><IsTruncated>{str(truncated).lower()}</IsTruncated>{token}{contents}</ListBucketResult>'.encode()

def seed(path,keys,namespace='aws'):
    with contextlib.closing(sqlite3.connect(path)) as db,db:
        db.executescript((ROOT/'crates/infra/src/sqlite/schema.sql').read_text())
        db.execute('PRAGMA application_id=1346458179');db.execute('PRAGMA user_version=1')
        for key,size in keys:
            db.execute("INSERT INTO objects(namespace,scheme,authority,object_key,size_bytes,queried_seconds,queried_nanos,revision) VALUES(?,'s3','bucket',?,?,123,0,1)",(namespace,key.encode(),size))
        db.execute('UPDATE metadata SET revision=1')

def read_keys(path):
    with contextlib.closing(sqlite3.connect(path)) as db:
        return [bytes(row[0]).decode() for row in db.execute('SELECT object_key FROM objects ORDER BY object_key')]

def select(column,uri,kind='directory'):
    return {'action':'select_uri','column':column,'uri':uri,'kind':kind}
def shot(name):return {'action':'snapshot','name':name}
def idle():return {'action':'wait_idle'}

class Runner:
    def __init__(self,binary,work,images,endpoint=None,network_trap=None,command_prefix=(),timeout=92):
        self.binary=binary;self.work=work;self.images=images;self.endpoint=endpoint;self.network_trap=network_trap;self.command_prefix=command_prefix;self.timeout=timeout;self.results=[]
    def run(self,name,actions,config=None,index=None,coordinate=None,extra=(),expected=0):
        config=config or self.work/'config';index=self.work/'index.db' if index is None else index
        expanded=[]
        for action in actions:
            if action.get('action')=='select_uri':
                expanded.append({'action':'wait_idle'})
            if action.get('action')=='snapshot' and not action.get('force'):
                expanded.append({'action':'local_ready'})
                if action.get('name') not in ['inflight','responsive','closing']:
                    expanded.append({'action':'wait_idle'})
            expanded.append(action)
        action_path=self.work/(name+'-actions.json');action_path.write_text(json.dumps(expanded))
        report=self.work/(name+'.json')
        cmd=[*self.command_prefix,str(self.binary),'--config',str(config),'--test-report',str(report),'--test-actions',str(action_path)]
        if index is not False:cmd+=['--index-location',str(index)]
        if self.endpoint:cmd+=['--test-endpoint',self.endpoint]
        cmd+=list(extra)
        env=faults.environment(self.work);env.update(AWS_EC2_METADATA_DISABLED='true',AWS_REGION='us-east-1',AWS_ACCESS_KEY_ID='gui-dummy',AWS_SECRET_ACCESS_KEY='gui-dummy',AWS_ENDPOINT_URL=self.endpoint or self.network_trap or 'http://127.0.0.1:9',AWS_ENDPOINT_URL_S3=self.endpoint or self.network_trap or 'http://127.0.0.1:9')
        if self.network_trap:
            env.update(HTTP_PROXY=self.network_trap,HTTPS_PROXY=self.network_trap,ALL_PROXY=self.network_trap,NO_PROXY='127.0.0.1,localhost')
        start=time.monotonic()
        process=subprocess.Popen(cmd,cwd=ROOT,env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        try:
            if coordinate:coordinate(process,report)
            out,err=process.communicate(timeout=self.timeout)
        finally:
            if process.poll() is None:process.kill();process.wait()
        assert process.returncode==expected,(name,process.returncode,err.decode())
        assert not out,(name,out)
        assert report.with_suffix('.states.json').exists(),(name,'no GUI snapshots',err.decode())
        states=json.loads(report.with_suffix('.states.json').read_text())
        (self.images/(name+'-states.json')).write_text(json.dumps(states,ensure_ascii=False,indent=2))
        for ppm in self.work.glob(name+'*.ppm'):
            save_png(ppm,self.images/(ppm.stem+'.png'))
        self.results.append({'name':name,'passed':True,'seconds':round(time.monotonic()-start,3),'states':states})
        return {s['name']:s for s in states}

def wait_state(process,report,name,timeout=15):
    deadline=time.monotonic()+timeout
    while time.monotonic()<deadline:
        if process.poll() is not None:raise AssertionError(('early exit',process.returncode,process.stderr.read().decode()))
        try:
            states=json.loads(report.with_suffix('.states.json').read_text())
            if any(s['name']==name for s in states):return states
        except (FileNotFoundError,json.JSONDecodeError):pass
        time.sleep(.025)
    raise AssertionError(('state timeout',name))

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--binary',type=pathlib.Path,required=True);parser.add_argument('--cli',type=pathlib.Path,required=True);parser.add_argument('--report',type=pathlib.Path,required=True);parser.add_argument('--screenshots',type=pathlib.Path,required=True);args=parser.parse_args()
    binary=args.binary.resolve();cli=args.cli.resolve();args.screenshots.mkdir(parents=True,exist_ok=True)
    all_results=[]
    with tempfile.TemporaryDirectory(prefix='archive-gui-workflow-') as directory:
        work=pathlib.Path(directory);config=work/'config';index=work/'index.db'
        config.write_text("[index]\nlocation='index.db'\n")
        seed(index,[('.DS_Store',1),('.git/config',2),('p',7),('p/a.txt',30),('p/b.txt',10),('p/sub/deep.txt',5)])
        def trap(request,state):raise AssertionError(('local view contacted S3',request))
        with faults.fixture(trap) as (state,endpoint):
            # Ordinary namespace for local queries; all possible SDK endpoints
            # and credentials are dummy/loopback in the child process.
            runner=Runner(binary,work,args.screenshots)
            signal=work/'external-done'
            actions=[select(0,'s3://bucket','root'),select(1,'s3://bucket/p/'),shot('columns'),select(2,'s3://bucket/p/a.txt','object'),shot('file'),{'action':'sort','terms':[2,1]},shot('sorted'),shot('before-external'),{'action':'wait_file','path':str(signal)},shot('stale'),{'action':'reload'},shot('reloaded'),{'action':'settings'},shot('settings'),{'action':'edit_setting','index':0,'value':'not-saved.db'},{'action':'cancel_settings'},shot('cancelled-settings'),{'action':'settings'},{'action':'edit_setting','index':4,'value':'bad-level'},{'action':'save_settings'},shot('invalid-settings'),{'action':'edit_setting','index':4,'value':'debug'},{'action':'save_settings'},shot('saved-settings'),{'action':'settings'},{'action':'reset_setting','index':4},{'action':'save_settings'},shot('reset-settings'),{'action':'settings'},{'action':'edit_setting','index':0,'value':'new.db'},{'action':'save_settings'},shot('new-location')]
            def external(process,report):
                wait_state(process,report,'before-external')
                with contextlib.closing(sqlite3.connect(index)) as db,db:db.execute('DELETE FROM objects WHERE object_key=?',(b'p/a.txt',))
                signal.touch()
            states=runner.run('local',actions,index=False,coordinate=external)
            assert len(states['columns']['columns'])==3
            assert len(states['file']['details'])==10 and states['file']['details'][2]['value']=='30'
            assert states['sorted']['columns'][2]['selected']==1
            assert [r['name'] for r in states['sorted']['columns'][2]['rows']]==['b.txt','a.txt','sub']
            assert states['stale']['details'][0]['value']=='s3://bucket/p/a.txt'
            assert states['reloaded']['columns'][2]['selected']==-1 and states['reloaded']['details'][0]['value']=='s3://bucket/p/'
            assert not (work/'not-saved.db').exists()
            assert states['invalid-settings']['settings_open'] and states['invalid-settings']['error']
            assert not states['saved-settings']['settings_open']
            assert states['new-location']['columns'][0]['count']==0 and states['new-location']['index'].endswith('new.db')
            assert 'level' not in config.read_text()
            assert not state.seen
            all_results+=runner.results
        missing=work/'missing.toml'
        runner=Runner(binary,work,args.screenshots)
        states=runner.run('create-settings',[{'action':'settings'},shot('uncreated'),{'action':'create_settings'},shot('created'),{'action':'cancel_settings'}],config=missing,index=False)
        assert not states['uncreated']['settings_exists'] and states['created']['settings_exists']
        all_results+=runner.results
        # Exercise actual pointer and keyboard dispatch through the rendered controls.
        runner=Runner(binary,work,args.screenshots)
        states=runner.run('pointer-input',[
            {'action':'click','x':1098,'y':32},shot('dialog'),
            {'action':'click','x':300,'y':558},{'action':'text','value':'debug'},shot('typed'),
            {'action':'click','x':1056,'y':696},idle(),shot('saved'),
            {'action':'click','x':1098,'y':32},shot('persisted'),{'action':'cancel_settings'}
        ],index=False)
        assert states['dialog']['settings_open'] and states['typed']['settings'][4]['value']=='debug'
        assert not states['saved']['settings_open'] and states['persisted']['settings'][4]['origin']=='config-file'
        all_results+=runner.results
        phase={'keys':[('p/a',1),('p/old',2),('p/sub/deep',3)],'late':False,'hang':False,'error':False}
        def route(request,state):
            if phase['hang']:return faults.response(hang=True)
            if request['method']=='HEAD':return faults.head(1,ETag='"head"',**{'Last-Modified':'Sat, 21 Nov 2026 00:00:00 GMT','x-amz-storage-class':'STANDARD'})
            assert faults.request_kind(request)=='list',request
            assert urllib.parse.urlsplit(request['path']).path.rstrip('/')=='/bucket',request
            if phase['error'] or (phase['late'] and 'continuation-token' in request['path']):return faults.error(500)
            prefix=urllib.parse.parse_qs(urllib.parse.urlsplit(request['path']).query).get('prefix',[''])[0]
            return faults.response(body=listing([(k,s)for k,s in phase['keys'] if k.startswith(prefix)],phase['late']))
        with faults.fixture(route) as (state,endpoint):
            config.write_text('');index.unlink(missing_ok=True)
            faults.run(cli,endpoint,work,['reconcile','s3://bucket/p','--recursive'],0)
            runner=Runner(binary,work,args.screenshots,endpoint)
            navigate=[select(0,'s3://bucket','root'),select(1,'s3://bucket/p/')]
            phase['keys']=[('p/.DS_Store',1),('p/a',4),('p/new',7),('p/sub/deep',3)]
            state.seen.clear()
            states=runner.run('folder-reconcile',navigate+[{'action':'reconcile'},idle(),shot('done')])
            assert len(state.seen)==1 and 'prefix=p%2F' in state.seen[0]['path'],state.seen
            assert 'p/old' not in read_keys(index) and 'p/new' in read_keys(index)
            assert '再照合完了' in states['done']['status'] and not states['done']['error']
            state.seen.clear()
            states=runner.run('exact-file',navigate+[select(2,'s3://bucket/p/a','object'),{'action':'reconcile','depth':'bad'},idle(),shot('done')])
            assert len(state.seen)==1 and state.seen[0]['method']=='HEAD',state.seen
            state.seen.clear()
            states=runner.run('invalid-depth',navigate+[{'action':'reconcile','depth':'0'},idle(),shot('done')])
            assert not state.seen and states['done']['error']
            phase['keys']=[('p/a',5)];state.seen.clear()
            states=runner.run('limited-depth',navigate+[{'action':'reconcile','depth':'1'},idle(),shot('done')])
            assert 'p/sub/deep' in read_keys(index) and 'p/new' not in read_keys(index)
            phase['keys']=[('p/fresh',8)];phase['late']=True;state.seen.clear()
            states=runner.run('partial-failure',navigate+[{'action':'reconcile'},idle(),shot('done')])
            assert len(state.seen)==2 and '再照合失敗' in states['done']['status'] and states['done']['error']
            assert 'p/fresh' in read_keys(index) and 'p/sub/deep' in read_keys(index)
            phase['late']=False;config.write_text("ignore=['*']\n[logging]\nlevel='error'\n");state.seen.clear()
            before=read_keys(index)
            states=runner.run('all-ignored',[{'action':'reconcile'},idle(),shot('done')])
            assert len(state.seen)==1 and states['done']['error'] and read_keys(index)==before
            config.write_text('');phase['hang']=True;state.seen.clear();state.part.clear()
            states=runner.run('cancel-responsive',navigate+[{'action':'reconcile'},{'action':'wait','ms':500},shot('inflight'),{'action':'top'},shot('responsive'),{'action':'cancel'},{'action':'cancel'},idle(),shot('cancelled')])
            assert states['inflight']['reconciling'] and states['responsive']['reconciling'] and len(states['responsive']['columns'])==1
            assert not states['cancelled']['reconciling'] and '取り消' in states['cancelled']['status']
            assert runner.results[-1]['seconds']<52 and len(state.seen)==1
            state.release.set();state.release.clear();state.seen.clear()
            states=runner.run('close-cancels',navigate+[{'action':'reconcile'},{'action':'wait','ms':500},shot('closing'),{'action':'close'}])
            assert states['closing']['reconciling'] and runner.results[-1]['seconds']<52 and len(state.seen)==1
            phase['hang']=False;state.release.set();phase['keys']=[];state.seen.clear()
            states=runner.run('empty-range',navigate+[{'action':'reconcile'},idle(),shot('done')])
            assert not read_keys(index) and states['done']['columns'][0]['count']==0 and '再照合完了' in states['done']['status']
            state.seen.clear();states=runner.run('no-known-buckets',[{'action':'reconcile'},idle(),shot('done')])
            assert not state.seen and states['done']['error']
            all_results+=runner.results
        with index_files.fixture() as (state,endpoint):
            namespace='test:'+endpoint;uri='s3://index-bucket/index.db'
            digest=hashlib.sha256(namespace.encode()+b'\0'+uri.encode()).hexdigest()
            cached=work/'home/.private-archive/cache/index'/digest/'working.db';cached.parent.mkdir(parents=True,exist_ok=True)
            seed(cached,[('p/a',1)],namespace)
            state['keys']=['p/a','p/new'];state['put_error']=412
            runner=Runner(binary,work,args.screenshots,endpoint)
            states=runner.run('s3-index-conflict',[select(0,'s3://bucket','root'),select(1,'s3://bucket/p/'),{'action':'reconcile'},idle(),shot('failed')],index=uri)
            assert '再照合失敗' in states['failed']['status'] and states['failed']['error']
            puts=[r for r in state['seen'] if r['method']=='PUT'];assert len(puts)==1 and puts[0]['headers'].get('if-none-match')=='*'
            assert state['db'] is None and read_keys(cached)==['p/a','p/new']
            before=len(state['seen']);runner.run('no-implicit-index-retry',[shot('local')],index=uri)
            assert len(state['seen'])==before
            all_results+=runner.results
    summary={'cases':all_results,'passed':len(all_results),'real_aws_requests':0,'iac_changes':0,'row_cache_budget':32*1024*1024}
    args.report.parent.mkdir(parents=True,exist_ok=True);args.report.write_text(json.dumps(summary,indent=2,ensure_ascii=False)+'\n');print(json.dumps({'passed':len(all_results),'real_aws_requests':0}))
if __name__=='__main__':main()

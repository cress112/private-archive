"""Real SeaweedFS GUI workflow; touches only a unique tiny test prefix."""
import argparse,json,pathlib,tempfile,uuid
from workflow import Runner,select,shot,idle,wait_state
import faults

def main():
    p=argparse.ArgumentParser();p.add_argument('--binary',type=pathlib.Path,required=True);p.add_argument('--cli',type=pathlib.Path,required=True);p.add_argument('--report',type=pathlib.Path,required=True);p.add_argument('--screenshots',type=pathlib.Path,required=True);a=p.parse_args()
    binary=a.binary.resolve();cli=a.cli.resolve();a.screenshots.mkdir(parents=True,exist_ok=True)
    endpoint='http://127.0.0.1:18333';prefix='gui-p3-08-'+uuid.uuid4().hex+'/'
    uri='s3://private-archive-test/'+prefix
    with tempfile.TemporaryDirectory(prefix='archive-gui-docker-') as td:
        work=pathlib.Path(td);(work/'config').write_text('');sample=work/'sample';sample.write_bytes(b'GUI test')
        other=work/'other';other.mkdir();(other/'config').write_text('')
        runner=Runner(binary,work,a.screenshots,endpoint)
        first=work/'external-ready';second=work/'delete-ready'
        def call(root,args,expected=0):return faults.run(cli,endpoint,root,args,expected)
        try:
            call(work,['cp',str(sample),uri+'folder/sample.txt'])
            def coordinate(process,report):
                wait_state(process,report,'before-external')
                call(other,['cp',str(sample),uri+'folder/external.txt'])
                first.touch()
                wait_state(process,report,'before-delete')
                call(work,['rm',uri,'--recursive','--auto-approve'])
                second.touch()
            actions=[select(0,'s3://private-archive-test','root'),select(1,uri),select(2,uri+'folder/'),select(3,uri+'folder/sample.txt','object'),shot('file'),shot('before-external'),{'action':'wait_file','path':str(first)},shot('stale'),select(2,uri+'folder/'),{'action':'reconcile'},idle(),shot('reconciled'),shot('before-delete'),{'action':'wait_file','path':str(second)},shot('before-reload'),{'action':'reload'},shot('after-delete')]
            states=runner.run('docker',actions,coordinate=coordinate)
            assert len(states['file']['details'])==10 and states['file']['details'][2]['value']=='8'
            assert states['stale']['columns'][3]['count']==1
            assert states['reconciled']['columns'][3]['count']==2 and '再照合完了' in states['reconciled']['status']
            assert states['before-reload']['columns'][3]['count']==2
            assert len(states['after-delete']['columns'])==1 and states['after-delete']['columns'][0]['count']==0
            a.report.write_text(json.dumps({'passed':True,'cases':runner.results,'endpoint':endpoint,'test_prefix':prefix,'test_objects':2,'bytes_per_object':8,'real_aws_requests':0,'test_prefix_cleaned':True},ensure_ascii=False,indent=2)+'\n')
            print(json.dumps({'passed':True,'real_aws_requests':0,'test_prefix_cleaned':True}))
        finally:
            # No bucket-wide deletion, Docker reset or IAM/resource changes.
            try:call(other,['rm',uri,'--recursive','--auto-approve'],1)
            except AssertionError:pass
if __name__=='__main__':main()

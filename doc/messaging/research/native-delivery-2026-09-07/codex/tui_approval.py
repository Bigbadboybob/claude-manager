from tui_fixture import *
import shlex
async def main():
 case='queue-tui-approval';env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port)
 p=pathlib.Path(env['CODEX_HOME'])/'config.toml';p.write_text(p.read_text().replace('approval_policy = "never"','approval_policy = "on-request"').replace('sandbox_mode = "danger-full-access"','sandbox_mode = "workspace-write"'))
 first=True;main_requests=[]
 async def handler(req,data,n):
  nonlocal first
  txt=json.dumps(data.get('input',[]))
  if 'Generate a concise, single-line task title' in txt:return await mock.reply(req,'Fixture title')
  main_requests.append({'at':time.time(),'body':data})
  if first:
   first=False
   cmd='python3 -c '+shlex.quote('print("APPROVED_FIXTURE_COMMAND")')
   script='text(await tools.exec_command('+json.dumps({'cmd':cmd,'sandbox_permissions':'require_escalated','justification':'Isolated approval probe','yield_time_ms':1000,'login':False})+'));'
   return await mock.reply(req,tool={'name':'exec','namespace':'functions','input':script})
  return await mock.reply(req,'MAIN_RESPONSE_'+str(len(main_requests)))
 mock.handler=handler;tui=await TUI(case,env,work).start();res={}
 try:
  await asyncio.sleep(2);await tui.send('APPROVAL_PROBE');await tui.send('\r');await tui.wait_text('Isolated approval probe',timeout=15)
  res['before_queue']=tui.snapshot('approval-before-queue')
  import sqlite3
  c=sqlite3.connect('file:'+env['CODEX_HOME']+'/state_5.sqlite?mode=ro',uri=True);tid=c.execute('select id from threads where source="cli" order by created_at desc limit 1').fetchone()[0];c.close();res['threadId']=tid
  q=await asyncio.create_subprocess_exec(BIN,'queue','--thread',tid,'--message','QUEUED_DURING_APPROVAL',env=env,cwd=work,stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE);out,err=await q.communicate();res['queue']={'code':q.returncode,'stdout':out.decode()}
  await asyncio.sleep(12);res['main_requests_while_approval']=len(main_requests);res['after_queue']=tui.snapshot('approval-after-queue')
  await tui.send('1');await tui.send('\r');await asyncio.sleep(3);res['main_requests_after_approval']=len(main_requests);res['after_approval']=tui.snapshot('approval-resolved');res['main_requests']=main_requests
  write_json(ROOT/case/'results.json',res);print({k:v for k,v in res.items() if k not in ['before_queue','after_queue','after_approval','main_requests']},flush=True)
 finally:await tui.close();await mock.close()
if __name__=='__main__':asyncio.run(main())

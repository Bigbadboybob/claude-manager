from tui_fixture import *
import sqlite3
async def main():
 case='queue-tui-reconnect';env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port);tui=await TUI(case,env,work).start();res={}
 try:
  await asyncio.sleep(2);await tui.send('INITIAL_HISTORY_MARKER');await tui.send('\r');await tui.wait_text('MOCK_RESPONSE_1')
  c=sqlite3.connect('file:'+env['CODEX_HOME']+'/state_5.sqlite?mode=ro',uri=True);tid=c.execute('select id from threads where source="cli" order by created_at desc limit 1').fetchone()[0];c.close();res['threadId']=tid
  await tui.close();tui=None;before=mock.responses
  q=await asyncio.create_subprocess_exec(BIN,'queue','--thread',tid,'--message','QUEUED_WHILE_CLIENT_CLOSED',env=env,cwd=work,stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE);out,err=await q.communicate();res['queue']={'code':q.returncode,'stdout':out.decode()};await asyncio.sleep(11);res['responses_while_closed']=mock.responses-before
  tui=await TUI(case,env,work,args=('resume',tid)).start();await asyncio.sleep(13);res['responses_after_resume']=mock.responses;res['after_resume']=tui.snapshot('after-resume')
  c=sqlite3.connect('file:'+env['CODEX_HOME']+'/queue_1.sqlite?mode=ro',uri=True);res['remaining_queue']=c.execute('select count(*) from queued_items where thread_id=?',(tid,)).fetchone()[0];c.close()
  write_json(ROOT/case/'results.json',res);print({k:v for k,v in res.items() if k!='after_resume'},flush=True)
 finally:
  if tui:await tui.close()
  await mock.close()
if __name__=='__main__':asyncio.run(main())

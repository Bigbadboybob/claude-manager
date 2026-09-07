from tui_fixture import *
import shlex
async def main():
 case='background-shell-tui';env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port);count=0;main=[]
 async def handler(req,data,n):
  nonlocal count
  if 'Generate a concise, single-line task title' in json.dumps(data.get('input',[])):return await mock.reply(req,'Fixture title')
  count+=1;main.append({'at':time.time(),'body':data})
  if count==1:
   cmd='python3 -c '+shlex.quote('import time; time.sleep(3); print("SHELL_COMPLETION_TUI")')
   return await mock.reply(req,tool={'name':'exec','namespace':'functions','input':'text(await tools.exec_command('+json.dumps({'cmd':cmd,'yield_time_ms':250,'login':False})+'));'})
  return await mock.reply(req,'MAIN_FINAL_'+str(count))
 mock.handler=handler;tui=await TUI(case,env,work).start();res={}
 try:
  await asyncio.sleep(2);await tui.send('START_WAITER');await tui.send('\r');await tui.wait_text('MAIN_FINAL_2');await tui.send('HUMAN_DRAFT_DURING_BACKGROUND_JOB');await asyncio.sleep(12)
  res={'main_model_requests_after_job_finished':count,'screen_after_job_finished':tui.snapshot('after-job-finished'),'main_requests':main}
  write_json(ROOT/case/'results.json',res);print({'main_model_requests_after_job_finished':count},flush=True)
 finally:await tui.close();await mock.close()
if __name__=='__main__':asyncio.run(main())

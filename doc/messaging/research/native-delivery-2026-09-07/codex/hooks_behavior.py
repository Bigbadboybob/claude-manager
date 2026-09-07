from harness import *
import shlex
async def run_case(case,active):
 env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port)
 p=pathlib.Path(env['CODEX_HOME'])/'config.toml';s=p.read_text().replace('hooks = false','hooks = true');s='bypass_hook_trust = true\n'+s;p.write_text(s)
 cmd='python3 '+shlex.quote(str(FIXTURE_DIR/'hook_fixture.py'))+' '+shlex.quote(str(ROOT/case/'hook-fired.txt'))
 write_json(pathlib.Path(env['CODEX_HOME'])/'hooks.json',{'hooks':{'PostToolUse':[{'matcher':'Bash','hooks':[{'type':'command','command':cmd,'async':True,'timeout':10}]}]}})
 rpc=await RPC(case,env,work).start();res={}
 async def handler(req,data,n):
  if n==1:return await mock.reply(req,tool={'name':'exec','namespace':'functions','input':'text(await tools.exec_command({cmd:"true",login:false}));'})
  if active and n==2:return await mock.reply(req,tool={'name':'exec','namespace':'functions','input':'text("NEXT_SAFE_CHECKPOINT");'},delay=4)
  return await mock.reply(req,'FINAL_'+str(n))
 mock.handler=handler
 try:
  t=await rpc.call('thread/start',{'cwd':str(work),'modelProvider':'mock','approvalPolicy':'never','sandbox':'danger-full-access','config':{'bypass_hook_trust':True}});tid=t['thread']['id']
  res['hooks_list']=await rpc.call('hooks/list',{'cwds':[str(work)]})
  await rpc.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'TRIGGER_HOOK'}]});await rpc.wait_event('turn/completed');await asyncio.sleep(5)
  res['requests_after_background_completion']=mock.responses;res['hook_ran']=(ROOT/case/'hook-fired.txt').exists()
  pos=len(rpc.events);await rpc.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'NEXT_USER_TURN'}]});await rpc.wait_event('turn/completed',after=pos)
  res['model_received_hook_at_requests']=[i+1 for i,r in enumerate(mock.requests) if 'HOOK_NOTIFICATION_PostToolUse' in json.dumps((r['body'] or {}).get('input',[]))]
  write_json(ROOT/case/'results.json',res);print(case,json.dumps(res),flush=True)
 finally:await rpc.close();await mock.close()
async def main():await asyncio.gather(run_case('hook-async-idle',False),run_case('hook-async-active',True))
if __name__=='__main__':asyncio.run(main())

from harness import *

async def run_case(case,active):
 env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port)
 rpc=await RPC(case,env,work).start();result={}
 toolscript='text(await tools.exec_command({cmd: \'python3 -c "import time; time.sleep(3); print(\\"SHELL_FINISHED\\")"\', yield_time_ms:250, max_output_tokens:1000, login:false}));'
 # Build the shell command with actual shell-safe quoting.
 import shlex
 command='python3 -c '+shlex.quote('import time; time.sleep(3); print("SHELL_FINISHED")')
 toolscript='text(await tools.exec_command('+json.dumps({'cmd':command,'yield_time_ms':250,'max_output_tokens':1000,'login':False})+'));'
 async def handler(req,data,n):
  if n==1:return await mock.reply(req,tool={'name':'exec','namespace':'functions','input':toolscript})
  if active and n==2:return await mock.reply(req,tool={'name':'exec','namespace':'functions','input':'text("SAFE_CHECKPOINT_AFTER_BACKGROUND_EXIT");'},delay=4)
  return await mock.reply(req,'FINAL_'+str(n))
 mock.handler=handler
 try:
  t=await rpc.call('thread/start',{'cwd':str(work),'modelProvider':'mock','approvalPolicy':'never','sandbox':'danger-full-access'});tid=t['thread']['id']
  await rpc.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'START_BACKGROUND_SHELL'}]})
  await rpc.wait_event('turn/completed',timeout=20);await asyncio.sleep(6)
  result={'threadId':tid,'responses_after_idle_wait':mock.responses,'events':[x for x in rpc.events if x.get('method') in ['turn/started','turn/completed','item/completed']]}
  pos=len(rpc.events)
  await rpc.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'NEXT_USER_TURN'}]});await rpc.wait_event('turn/completed',after=pos)
  result['responses_after_next_user_turn']=mock.responses
  write_json(ROOT/case/'results.json',result);print(case,json.dumps({'responses_idle':result['responses_after_idle_wait'],'responses_final':mock.responses}),flush=True)
 finally:await rpc.close();await mock.close()
async def main():await run_case('background-shell-idle',False);await run_case('background-shell-active',True)
if __name__=='__main__':asyncio.run(main())

from harness import *

def tools_in(data):
 all=[]
 for t in data.get('tools',[]):all.append((None,t))
 for i in data.get('input',[]):
  if i.get('type')=='additional_tools':
   for ns in i.get('tools',[]):
    if ns.get('type')=='namespace':all.extend((ns['name'],t) for t in ns['tools'])
    else:all.append((None,ns))
 return all

async def run_case(case,delay,code_mode,yield_early):
 env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port)
 p=pathlib.Path(env['CODEX_HOME'])/'config.toml';s=p.read_text().replace('[features]','[features]\ncode_mode = '+str(code_mode).lower())
 s+='\n[mcp_servers.fixture]\ncommand = "/usr/bin/python3"\nargs = '+json.dumps([str(FIXTURE_DIR/'mcp_fixture.py'),str(ROOT/case/'mcp-events.jsonl')])+'\nstartup_timeout_sec = 10\ntool_timeout_sec = 180\n'
 p.write_text(s)
 rpc=await RPC(case,env,work).start();result={};first=True
 async def handler(req,data,n):
  nonlocal first
  if first:
   first=False;result['tools']=[(ns,t.get('name'),t.get('type')) for ns,t in tools_in(data)]
   if code_mode:
    script='text(await tools.mcp__fixture__wait_event('+json.dumps({'delay_s':delay,'label':case})+'));'
    if yield_early:script='// @exec: {"yield_time_ms":100}\n'+script
    else:script='// @exec: {"yield_time_ms":180000}\n'+script
    return await mock.reply(req,tool={'name':'exec','namespace':'functions','input':script})
   raise ValueError('Only the verified code-mode MCP fixture is supported; set code_mode=True')
  return await mock.reply(req,'FINAL_'+str(n))
 mock.handler=handler
 try:
  t=await rpc.call('thread/start',{'cwd':str(work),'modelProvider':'mock','approvalPolicy':'never','sandbox':'danger-full-access'});tid=t['thread']['id'];start=time.time()
  await rpc.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'START_MCP_WAITER'}]})
  done=await rpc.wait_event('turn/completed',timeout=delay+30)
  result['first_turn_seconds']=done['at']-start;result['responses_at_first_turn_end']=mock.responses
  await asyncio.sleep(delay+2 if yield_early else 2)
  result['responses_after_completion_wait']=mock.responses
  result['request_offsets']=[r['at']-start for r in mock.requests]
  write_json(ROOT/case/'results.json',result);print(case,json.dumps(result),flush=True)
 finally:await rpc.close();await mock.close()
async def main():
 await asyncio.gather(run_case('mcp-code-long-hold',130,True,False),run_case('mcp-code-yielded',4,True,True))
if __name__=='__main__':asyncio.run(main())

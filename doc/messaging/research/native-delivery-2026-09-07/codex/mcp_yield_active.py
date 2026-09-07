from harness import *
async def main():
 case='mcp-code-yielded-active';env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port)
 p=pathlib.Path(env['CODEX_HOME'])/'config.toml';p.write_text(p.read_text()+'\n[mcp_servers.fixture]\ncommand = "/usr/bin/python3"\nargs = '+json.dumps([str(FIXTURE_DIR/'mcp_fixture.py'),str(ROOT/case/'mcp-events.jsonl')])+'\ntool_timeout_sec = 20\n')
 rpc=await RPC(case,env,work).start()
 async def handler(req,data,n):
  if n==1:return await mock.reply(req,tool={'name':'exec','namespace':'functions','input':'// @exec: {"yield_time_ms":100}\nconst r=await tools.mcp__fixture__wait_event({delay_s:3,label:"ACTIVE_MCP"});store("waiter_result",r);text(r);'})
  if n==2:return await mock.reply(req,tool={'name':'exec','namespace':'functions','input':'text("SAFE_CHECKPOINT_AFTER_MCP_EXIT");'},delay=4)
  if n==4:return await mock.reply(req,tool={'name':'exec','namespace':'functions','input':'text(load("waiter_result"));'})
  return await mock.reply(req,'FINAL_'+str(n))
 mock.handler=handler
 try:
  t=await rpc.call('thread/start',{'cwd':str(work),'modelProvider':'mock','approvalPolicy':'never','sandbox':'danger-full-access'});tid=t['thread']['id'];await rpc.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'START_ACTIVE_WAITER'}]});await rpc.wait_event('turn/completed');await asyncio.sleep(3)
  res={'requests_after_active_and_idle':mock.responses,'model_received_mcp_at_requests_before_explicit_read':[i+1 for i,r in enumerate(mock.requests) if 'MCP_EVENT_ACTIVE_MCP' in json.dumps((r['body'] or {}).get('input',[]))]}
  pos=len(rpc.events);await rpc.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'READ_FINISHED_WAITER_RESULT'}]});await rpc.wait_event('turn/completed',after=pos)
  res['model_received_mcp_after_explicit_read']=[i+1 for i,r in enumerate(mock.requests) if 'MCP_EVENT_ACTIVE_MCP' in json.dumps((r['body'] or {}).get('input',[]))]
  write_json(ROOT/case/'results.json',res);print(res,flush=True)
 finally:await rpc.close();await mock.close()
if __name__=='__main__':asyncio.run(main())

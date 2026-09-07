"""Compare documented standalone tool output with context-only injection."""
from harness import *

async def main():
 case='native-structured-input';env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port);rpc=await RPC(case,env,work).start();result={}
 try:
  t=await rpc.call('thread/start',{'cwd':str(work),'modelProvider':'mock','approvalPolicy':'never','sandbox':'danger-full-access'});tid=t['thread']['id']
  await rpc.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'BASELINE'}]});await rpc.wait_event('turn/completed')
  before=mock.responses
  await rpc.call('thread/inject_items',{'threadId':tid,'items':[{'type':'message','role':'user','content':[{'type':'input_text','text':'CM_CONTEXT_ONLY'}]}]})
  await asyncio.sleep(2);result['inject_items_started_turn']=mock.responses>before
  pos=len(rpc.events);start=time.time()
  await rpc.call('turn/start',{'threadId':tid,'input':[],'toolOutput':{'name':'cm_notification','namespace':None,'output':'CM_STANDALONE_IDLE_EVENT'}})
  done=await rpc.wait_event('turn/completed',after=pos)
  result['idle_tool_output_woke']=mock.responses>before;result['idle_tool_output_completion_s']=done['at']-start
  result['context_visible_after_next_turn']='CM_CONTEXT_ONLY' in json.dumps(mock.requests[-1]['body'].get('input'))
  result['idle_tool_output_items']=[i for i in mock.requests[-1]['body'].get('input',[]) if 'CM_STANDALONE_IDLE_EVENT' in json.dumps(i)]
  first=True
  async def handler(req,data,n):
   nonlocal first
   if first:first=False;return await mock.reply(req,tool={'name':'exec','namespace':'functions','input':'text(await tools.exec_command({cmd:"sleep 3; echo CM_FOREGROUND_DONE",yield_time_ms:10000}));'})
   return await mock.reply(req,'ACTIVE_DONE')
  mock.handler=handler
  pos=len(rpc.events);turn=await rpc.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'ACTIVE_BASELINE'}]})
  await rpc.wait_event('item/started',after=pos,predicate=lambda e:e['params']['item']['type']=='commandExecution')
  out=await rpc.call('turn/start',{'threadId':tid,'input':[],'toolOutput':{'name':'cm_notification','namespace':None,'output':'CM_STANDALONE_ACTIVE_EVENT'}})
  result['active_returned_same_turn']=out['turn']['id']==turn['turn']['id']
  await rpc.wait_event('turn/completed',after=pos)
  result['active_tool_output_in_next_request']='CM_STANDALONE_ACTIVE_EVENT' in json.dumps(mock.requests[-1]['body'].get('input'))
  result['foreground_tool_completed']='CM_FOREGROUND_DONE' in json.dumps(mock.requests[-1]['body'].get('input'))
  write_json(ROOT/case/'results.json',result);print(json.dumps(result,indent=2),flush=True)
 finally:await rpc.close();await mock.close()
if __name__=='__main__':asyncio.run(main())

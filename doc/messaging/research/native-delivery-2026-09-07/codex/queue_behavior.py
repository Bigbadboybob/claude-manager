from harness import *

async def main():
 case='queue-appserver';env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port)
 primary=await RPC(case,env,work).start()
 (ROOT/(case+'-second')).mkdir(exist_ok=True)
 second=await RPC(case+'-second',env,work).start()
 result={}
 try:
  t=await primary.call('thread/start',{'cwd':str(work),'modelProvider':'mock','approvalPolicy':'never','sandbox':'danger-full-access'});tid=t['thread']['id'];result['threadId']=tid
  await primary.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'SETUP'}]})
  await primary.wait_event('turn/completed')
  result['secondary_loaded']=await second.call('thread/loaded/list')
  pos=len(primary.events);count=mock.responses
  q={'threadId':tid,'input':[{'type':'text','text':'QUEUED_IDLE'}],'clientUserMessageId':'fixture-idle-1'}
  result['idle_add']=await second.call('thread/queue/add',q)
  await asyncio.sleep(3)
  result['idle_after_3s']={'response_delta':mock.responses-count,'events':[x for x in primary.events[pos:] if x.get('method') in ['turn/started','turn/completed','thread/queue/changed']],'queue':await second.call('thread/queue/list',{'threadId':tid})}
  result['same_id_retry_after_3s']=await second.call('thread/queue/add',q)
  await asyncio.sleep(2)
  result['retry_after_delivery']={'responses':mock.responses,'queue':await second.call('thread/queue/list',{'threadId':tid})}
  async def slow(request,data,n):return await mock.reply(request,'SLOW_RESPONSE_'+str(n),delay=2)
  mock.handler=slow
  pos=len(primary.events);count=mock.responses
  await primary.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'ACTIVE_WORK'}]})
  await asyncio.sleep(.3)
  q={'threadId':tid,'input':[{'type':'text','text':'QUEUED_WHILE_ACTIVE'}],'clientUserMessageId':'fixture-active-1'}
  result['active_add_1']=await second.call('thread/queue/add',q)
  result['active_add_retry']=await second.call('thread/queue/add',q)
  result['active_queue_before_finish']=await second.call('thread/queue/list',{'threadId':tid})
  await asyncio.sleep(6)
  result['active_after_6s']={'response_delta':mock.responses-count,'events':[x for x in primary.events[pos:] if x.get('method') in ['turn/started','turn/completed','thread/queue/changed']],'queue':await second.call('thread/queue/list',{'threadId':tid})}
  write_json(ROOT/case/'results.json',result);print(json.dumps(result,indent=2),flush=True)
 finally:await second.close();await primary.close();await mock.close()

if __name__=='__main__':asyncio.run(main())

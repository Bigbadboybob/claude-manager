from harness import *
async def main():
 case='queue-latency';env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port)
 a=await RPC(case,env,work).start();(ROOT/(case+'-second')).mkdir(exist_ok=True);b=await RPC(case+'-second',env,work).start();res=[]
 try:
  t=await a.call('thread/start',{'cwd':str(work),'modelProvider':'mock','approvalPolicy':'never','sandbox':'danger-full-access'});tid=t['thread']['id']
  await a.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'SETUP'}]});await a.wait_event('turn/completed')
  for who,rpc in [('same-process',a),('cross-process-1',b),('cross-process-2',b)]:
   pos=len(a.events);start=time.time();q=await rpc.call('thread/queue/add',{'threadId':tid,'input':[{'type':'text','text':who}],'clientUserMessageId':who})
   try:
    done=await a.wait_event('turn/completed',after=pos,timeout=18)
    result={'case':who,'latency_s':done['at']-start,'queued':q,'delivered':True}
   except TimeoutError:result={'case':who,'delivered':False}
   res.append(result);print(result,flush=True);await asyncio.sleep(.4)
  write_json(ROOT/case/'results.json',res)
 finally:await b.close();await a.close();await mock.close()
if __name__=='__main__':asyncio.run(main())

from harness import *
import websockets,shlex
class WSRPC(RPC):
 def __init__(self,case,env,work,sock,label):super().__init__(case,env,work);self.sock=sock;self.label=label
 async def start(self):
  self.ws=await websockets.unix_connect(self.sock,uri='ws://localhost',compression=None,open_timeout=5)
  self.reader=asyncio.create_task(self.read());await self.call('initialize',{'clientInfo':{'name':'cm_fixture_'+self.label,'version':'1'},'capabilities':{'experimentalApi':True}});await self.send({'method':'initialized'});return self
 async def send(self,m):await self.ws.send(json.dumps(m))
 async def read(self):
  try:
   async for l in self.ws:
    m=json.loads(l);self.events.append({'at':time.time(),**m});write_json(ROOT/self.case/(self.label+'-events.json'),self.events)
    if m.get('id') in self.pending and ('result' in m or 'error' in m):self.pending.pop(m['id']).set_result(m)
  except websockets.ConnectionClosed:pass
 async def close(self):await self.ws.close();await self.reader
async def main():
 case='native-appserver-control';env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port);sock=str(ROOT/case/'app.sock');log=open(ROOT/case/'server.stderr.log','wb')
 server=await asyncio.create_subprocess_exec(BIN,'app-server','--listen','unix://'+sock,env=env,cwd=work,stdout=log,stderr=log,start_new_session=True)
 for i in range(100):
  if pathlib.Path(sock).exists():break
  await asyncio.sleep(.05)
 clients=[];res={}
 try:
  a=await WSRPC(case,env,work,sock,'a').start();clients.append(a)
  t=await a.call('thread/start',{'cwd':str(work),'modelProvider':'mock','approvalPolicy':'never','sandbox':'danger-full-access'});tid=t['thread']['id'];res['threadId']=tid
  async def handler(req,data,n):
   if n==1:
    cmd='python3 -c '+shlex.quote('import time; time.sleep(3); print("LONG_TOOL_COMPLETED")')
    return await mock.reply(req,tool={'name':'exec','namespace':'functions','input':'text(await tools.exec_command('+json.dumps({'cmd':cmd,'yield_time_ms':5000,'login':False})+'));'})
   return await mock.reply(req,'RESPONSE_'+str(n))
  mock.handler=handler
  tr=await a.call('turn/start',{'threadId':tid,'input':[{'type':'text','text':'PRIMARY_WORK'}]});turnid=tr['turn']['id']
  await a.wait_event('item/started',predicate=lambda x:x['params']['item']['type']=='commandExecution')
  b=await WSRPC(case,env,work,sock,'b').start();clients.append(b)
  resumed=await b.call('thread/resume',{'threadId':tid});res['resume_rejoins']={'same_id':resumed['thread']['id']==tid,'status':resumed['thread']['status'],'loaded':await b.call('thread/loaded/list')}
  pos=len(b.events);res['steer']=await b.call('turn/steer',{'threadId':tid,'expectedTurnId':turnid,'clientUserMessageId':'steer-one','input':[{'type':'text','text':'NATIVE_STEER_MESSAGE'}]})
  await a.close();clients.remove(a);done=await b.wait_event('turn/completed',timeout=12);res['turn_after_original_disconnect']=done
  try:await b.call('turn/steer',{'threadId':tid,'expectedTurnId':turnid,'input':[{'type':'text','text':'STALE_SHOULD_FAIL'}]})
  except RuntimeError as e:res['stale_steer_error']=str(e)
  # Idle wake through the same server, with no terminal involved.
  pos=len(b.events);start=time.time();wake=await b.call('turn/start',{'threadId':tid,'clientUserMessageId':'native-idle-wake','input':[{'type':'text','text':'NATIVE_IDLE_WAKE'}]});await b.wait_event('turn/completed',after=pos);res['idle_wake_s']=time.time()-start
  await b.close();clients.remove(b);await asyncio.sleep(1)
  c=await WSRPC(case,env,work,sock,'c').start();clients.append(c);r=await c.call('thread/resume',{'threadId':tid});res['reconnect']={'same_id':r['thread']['id']==tid,'status':r['thread']['status'],'turn_count':len(r['thread']['turns'])}
  write_json(ROOT/case/'results.json',res);print(json.dumps({k:v for k,v in res.items() if k!='turn_after_original_disconnect'},indent=2),flush=True)
 finally:
  for c in clients:await c.close()
  os.killpg(server.pid,signal.SIGTERM)
  try:await asyncio.wait_for(server.wait(),3)
  except asyncio.TimeoutError:os.killpg(server.pid,signal.SIGKILL);await server.wait()
  log.close();await mock.close()
if __name__=='__main__':asyncio.run(main())

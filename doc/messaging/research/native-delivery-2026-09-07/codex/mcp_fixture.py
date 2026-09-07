import asyncio,json,sys,time,pathlib
log=pathlib.Path(sys.argv[1])
def send(x):print(json.dumps(x),flush=True)
def note(x):
 with log.open('a') as f:f.write(json.dumps({'at':time.time(),**x})+'\n')
async def handle(m):
 note({'received':m});rid=m.get('id');method=m.get('method');params=m.get('params',{})
 if method=='initialize':send({'jsonrpc':'2.0','id':rid,'result':{'protocolVersion':params.get('protocolVersion','2025-03-26'),'capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}})
 elif method=='tools/list':send({'jsonrpc':'2.0','id':rid,'result':{'tools':[{'name':'wait_event','description':'Isolated notification waiter; returns when the fixture event fires.','inputSchema':{'type':'object','properties':{'delay_s':{'type':'number'},'label':{'type':'string'}},'required':['delay_s','label']},'annotations':{'readOnlyHint':True,'destructiveHint':False,'openWorldHint':False}}]}})
 elif method=='tools/call':
  args=params['arguments'];await asyncio.sleep(args['delay_s']);result={'content':[{'type':'text','text':'MCP_EVENT_'+args['label']}]};note({'completed':rid,'result':result});send({'jsonrpc':'2.0','id':rid,'result':result})
 elif rid is not None:send({'jsonrpc':'2.0','id':rid,'result':{}})
async def main():
 tasks=set()
 while True:
  line=await asyncio.to_thread(sys.stdin.readline)
  if not line:break
  try:m=json.loads(line)
  except ValueError:continue
  t=asyncio.create_task(handle(m));tasks.add(t);t.add_done_callback(tasks.discard)
 await asyncio.gather(*tasks)
asyncio.run(main())

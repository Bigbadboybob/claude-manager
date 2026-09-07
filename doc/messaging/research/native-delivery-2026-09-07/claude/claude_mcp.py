"""Tiny stdio MCP fixture with file-triggered waits and push events."""
import sys,json,time,threading,os,socket
from pathlib import Path
root=Path(sys.argv[1]);root.mkdir(parents=True,exist_ok=True)
lock=threading.Lock()
def log(x):
 with (root/'mcp-events.jsonl').open('a') as f:f.write(json.dumps({'at':time.time(),**x})+'\n')
def send(x):
 with lock:print(json.dumps({'jsonrpc':'2.0',**x}),flush=True)
def reply(i,r):send({'id':i,'result':r})
def wait_file(path):
 while not path.exists():time.sleep(.05)
 return path.read_text()
def push():
 seen=set()
 while True:
  for p in sorted(root.glob('event-*.json')):
   if p.name in seen:continue
   seen.add(p.name);obj=json.loads(p.read_text());log({'event':obj})
   if obj.get('method')=='fixture/exit':
    p.unlink();os._exit(17)
   if obj.get('method')=='fixture/socket':
    ep=os.environ.get('CLAUDE_CODE_MESSAGING_SOCKET');token=os.environ.get('CLAUDE_CODE_MESSAGING_TOKEN');log({'socket_present':bool(ep),'token_present':bool(token)})
    sock=socket.socket(socket.AF_UNIX);sock.connect(ep.removeprefix('uds:'))
    if token:sock.sendall((json.dumps({'type':'auth','token':token})+'\n').encode())
    sock.sendall((json.dumps({'type':'user','from':'cm-fixture','message':{'role':'user','content':obj['params']['content']}})+'\n').encode());sock.close()
   else:send(obj)
  time.sleep(.05)
def call(m):
 name=m['params']['name'];a=m['params'].get('arguments',{})
 log({'call':name,'args':a})
 if name=='wait':result=wait_file(root/'release')
 elif name=='post_self':
  ep=os.environ.get('CLAUDE_CODE_MESSAGING_SOCKET');token=os.environ.get('CLAUDE_CODE_MESSAGING_TOKEN')
  log({'socket_present':bool(ep),'token_present':bool(token)})
  s=socket.socket(socket.AF_UNIX);s.connect(ep.removeprefix('uds:'))
  if token:s.sendall((json.dumps({'type':'auth','token':token})+'\n').encode())
  s.sendall((json.dumps({'type':'user','from':'cm-fixture','message':{'role':'user','content':a.get('text','SELF_SOCKET_EVENT')}})+'\n').encode());s.close();result='POSTED'
 else:result='fixture result'
 reply(m['id'],{'content':[{'type':'text','text':result}]});log({'completed':name})
for line in sys.stdin:
 try:m=json.loads(line)
 except ValueError:continue
 log({'method':m.get('method'),**({'initialize':m.get('params')} if m.get('method')=='initialize' else {}),**({'reply':m} if 'result' in m or 'error' in m else {})})
 if m.get('method')=='initialize':reply(m['id'],{'protocolVersion':'2024-11-05','serverInfo':{'name':'cm-fixture','version':'1'},'capabilities':{'tools':{},'resources':{'subscribe':True},'logging':{},'experimental':{'claude/channel':{}}},'instructions':'Local notification delivery fixture.'})
 elif m.get('method')=='notifications/initialized':threading.Thread(target=push,daemon=True).start()
 elif m.get('method')=='tools/list':reply(m['id'],{'tools':[{'name':n,'description':'Local fixture '+n,'inputSchema':{'type':'object','properties':{'text':{'type':'string'}}}} for n in ['wait','post_self']]})
 elif m.get('method')=='tools/call':threading.Thread(target=call,args=(m,),daemon=True).start()
 elif m.get('method')=='resources/list':reply(m['id'],{'resources':[]})
 elif m.get('method')=='resources/templates/list':reply(m['id'],{'resourceTemplates':[]})
 elif 'id' in m:reply(m['id'],{})

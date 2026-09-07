"""Isolated real Claude client + local deterministic model, no paid calls."""
import os,sys,json,time,uuid,threading,subprocess,signal,pty,select,fcntl,termios,struct
from pathlib import Path
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
HERE=Path(__file__).resolve().parent
ROOT=Path(os.environ.get('CM_RESEARCH_OUTPUT','/tmp/cm-native-delivery-research'))
BIN=os.environ.get('CM_RESEARCH_CLAUDE_BIN',str(Path.home()/'.local/share/claude/versions/2.1.263'))
class Mock:
 def __init__(self,case,reply=None):
  self.root=ROOT/case; self.root.mkdir(parents=True,exist_ok=True);self.requests=[];self.reply=reply;self.n=0
  mock=self
  class Handler(BaseHTTPRequestHandler):
   def log_message(self,*a):pass
   def do_GET(self):self.send_response(200);self.end_headers();self.wfile.write(b'{}')
   def do_POST(self):
    raw=self.rfile.read(int(self.headers.get('Content-Length','0')))
    try:body=json.loads(raw)
    except ValueError:body={}
    mock.requests.append({'at':time.time(),'path':self.path,'body':body})
    (mock.root/'requests.json').write_text(json.dumps(mock.requests,indent=2))
    if 'count_tokens' in self.path:
     self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(b'{"input_tokens":100}');return
    if '/messages' not in self.path:
     self.send_response(200);self.end_headers();self.wfile.write(b'{}');return
    mock.n+=1
    params=mock.reply(body,mock.n) if mock.reply else {}
    delay=params.get('delay',0);tool=params.get('tool');text=params.get('text','MOCK_DONE_'+str(mock.n))
    block={'type':'tool_use','id':'toolu_'+uuid.uuid4().hex,'name':tool[0],'input':tool[1]} if tool else {'type':'text','text':text}
    msg={'id':'msg_'+uuid.uuid4().hex,'type':'message','role':'assistant','model':body.get('model','claude-sonnet-4-6'),'content':[block],'stop_reason':'tool_use' if tool else 'end_turn','stop_sequence':None,'usage':{'input_tokens':100,'output_tokens':20}}
    if not body.get('stream'):
     self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(json.dumps(msg).encode());return
    self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Cache-Control','no-cache');self.end_headers()
    def emit(t,**kw):
     try:self.wfile.write(('event: '+t+'\ndata: '+json.dumps({'type':t,**kw})+'\n\n').encode());self.wfile.flush()
     except (BrokenPipeError,ConnectionResetError):pass
    emit('message_start',message={**msg,'content':[],'stop_reason':None,'usage':{'input_tokens':100,'output_tokens':0}})
    if delay:time.sleep(delay)
    emit('content_block_start',index=0,content_block={**block,**({'input':{}} if tool else {'text':''})})
    emit('content_block_delta',index=0,delta={'type':'input_json_delta','partial_json':json.dumps(block['input'])} if tool else {'type':'text_delta','text':text})
    emit('content_block_stop',index=0)
    emit('message_delta',delta={'stop_reason':msg['stop_reason'],'stop_sequence':None},usage={'output_tokens':20})
    emit('message_stop')
  self.server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
  threading.Thread(target=self.server.serve_forever,daemon=True).start()
 def close(self):self.server.shutdown();self.server.server_close()
class Client:
 def __init__(self,case,mock,extra=None,env_extra=None,settings=None,interactive=True):
  self.root=ROOT/case;self.work=self.root/'work';self.work.mkdir(parents=True,exist_ok=True);self.config=self.root/'config';self.config.mkdir(exist_ok=True);self.buf=b'';self.events=[]
  self.session=str(uuid.uuid4())
  (self.root/'debug.log').unlink(missing_ok=True)
  (self.config/'.claude.json').write_text(json.dumps({'hasCompletedOnboarding':True,'theme':'dark','customApiKeyResponses':{'approved':['cm-isolated-fixture'], 'rejected':[]},'projects':{str(self.work):{'hasTrustDialogAccepted':True}}}))
  env={'HOME':str(self.root/'home'),'CLAUDE_CONFIG_DIR':str(self.config),'PATH':'/usr/local/bin:/usr/bin:/bin','SHELL':'/bin/bash','TERM':'xterm-256color','LANG':'C.UTF-8','USER':'lucas','ANTHROPIC_API_KEY':'cm-isolated-fixture','ANTHROPIC_BASE_URL':'http://127.0.0.1:'+str(mock.server.server_port),'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC':'1','DISABLE_AUTOUPDATER':'1','DISABLE_TELEMETRY':'1','DISABLE_ERROR_REPORTING':'1','ENABLE_TOOL_SEARCH':'false',**(env_extra or {})}
  Path(env['HOME']).mkdir(exist_ok=True)
  cfg={'permissions':{'defaultMode':'bypassPermissions'},'skipDangerousModePermissionPrompt':True,'autoUpdatesChannel':'stable',**(settings or {})}
  args=[BIN,'--model','claude-sonnet-4-6','--session-id',self.session,'--setting-sources','','--settings',json.dumps(cfg),'--strict-mcp-config','--mcp-config','{"mcpServers":{}}','--system-prompt','You are a deterministic local test fixture. No external services.','--debug-file',str(self.root/'debug.log'),*(extra or [])]
  self.interactive=interactive
  if interactive:
   master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',36,120,0,0));self.fd=master
   self.proc=subprocess.Popen(args,stdin=slave,stdout=slave,stderr=slave,env=env,cwd=self.work,start_new_session=True);os.close(slave)
   threading.Thread(target=self.read_pty,daemon=True).start()
  else:
   self.proc=subprocess.Popen([*args,'-p','--input-format','stream-json','--output-format','stream-json','--verbose'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=open(self.root/'stderr.log','wb'),env=env,cwd=self.work,start_new_session=True)
   threading.Thread(target=self.read_pipe,daemon=True).start()
 def read_pty(self):
  while self.proc.poll() is None:
   try:
    if not select.select([self.fd],[],[],.2)[0]:continue
    data=os.read(self.fd,65536)
    if not data:break
    self.buf+=data;(self.root/'terminal.raw').write_bytes(self.buf)
    if b'\x1b[6n' in data:os.write(self.fd,b'\x1b[1;1R')
   except OSError:break
 def read_pipe(self):
  for l in self.proc.stdout:
   try:self.events.append(json.loads(l))
   except ValueError:self.events.append({'raw':l.decode(errors='replace')})
   (self.root/'events.json').write_text(json.dumps(self.events,indent=2))
 def send(self,text):
  if self.interactive:os.write(self.fd,text.encode())
  else:self.proc.stdin.write((json.dumps({'type':'user','message':{'role':'user','content':text}})+'\n').encode());self.proc.stdin.flush()
 def close(self):
  if self.proc.poll() is None:
   os.killpg(self.proc.pid,signal.SIGTERM)
   try:self.proc.wait(3)
   except subprocess.TimeoutExpired:os.killpg(self.proc.pid,signal.SIGKILL);self.proc.wait()
  if self.interactive:os.close(self.fd)
def wait(pred,seconds=15):
 end=time.monotonic()+seconds
 while time.monotonic()<end:
  if pred():return True
  time.sleep(.05)
 return False
if __name__=='__main__':
 mock=Mock('claude-smoke');client=Client('claude-smoke',mock)
 try:
  time.sleep(3);print(client.buf.decode(errors='replace')[-6000:],flush=True)
  client.send('FIXTURE_SMOKE\r');wait(lambda:mock.n>0,10);time.sleep(2);print('REQUESTS',mock.n,flush=True);print(client.buf.decode(errors='replace')[-6000:],flush=True)
 finally:client.close();mock.close()

import asyncio, json, os, pathlib, subprocess, time, uuid, signal, sys, shutil
from aiohttp import web
ROOT=pathlib.Path(os.environ.get('CM_CODEX_RESEARCH_ROOT','/tmp/cm-codex-notification-research'))
FIXTURE_DIR=pathlib.Path(__file__).resolve().parent
BIN=os.environ.get('CM_CODEX_BIN') or shutil.which('codex')
if not BIN: raise RuntimeError('Set CM_CODEX_BIN to the installed Codex executable')

def write_json(path,x): pathlib.Path(path).write_text(json.dumps(x,indent=2,ensure_ascii=False))

def make_env(case):
    home=ROOT/case/'home'; home.mkdir(parents=True,exist_ok=True)
    ch=home/'.codex'; ch.mkdir(exist_ok=True)
    work=ROOT/case/'work';work.mkdir(exist_ok=True)
    env={'HOME':str(home),'CODEX_HOME':str(ch),'PATH':'/usr/local/bin:/usr/bin:/bin','LANG':'C.UTF-8','TERM':'xterm-256color','USER':'lucas','LOGNAME':'lucas','SHELL':'/bin/bash','RUST_LOG':'codex_core=debug,codex_app_server=debug','CODEX_QUIET_MODE':'1'}
    return env,work

class Mock:
    def __init__(self,case):
        self.case=case;self.requests=[];self.events=[];self.responses=0;self.handler=None
        self.app=web.Application(); self.app.router.add_route('*','/{tail:.*}',self.handle)
    async def start(self):
        self.runner=web.AppRunner(self.app);await self.runner.setup()
        self.site=web.TCPSite(self.runner,'127.0.0.1',0);await self.site.start()
        self.port=self.site._server.sockets[0].getsockname()[1]
        return self
    async def close(self):await self.runner.cleanup()
    def save(self):write_json(ROOT/self.case/'requests.json',self.requests)
    async def handle(self,request):
        body=await request.read()
        try:data=json.loads(body) if body else None
        except ValueError:data={'raw':body.decode(errors='replace')}
        rec={'at':time.time(),'method':request.method,'path':request.path,'body':data}
        self.requests.append(rec);self.save()
        if request.method=='GET' and 'responses' in request.path:
            return web.Response(status=426,text='Use HTTP SSE')
        if request.method=='GET':return web.json_response({'data':[],'models':[]})
        if request.path.endswith('/responses'):
            self.responses+=1
            if self.handler:return await self.handler(request,data,self.responses)
            return await self.reply(request,'MOCK_RESPONSE_'+str(self.responses))
        return web.json_response({'error':{'message':'unimplemented fixture endpoint'}},status=404)
    async def reply(self,request,text=None,tool=None,delay=0):
        rid='resp_'+uuid.uuid4().hex
        r=web.StreamResponse(status=200,headers={'Content-Type':'text/event-stream','Cache-Control':'no-cache'});await r.prepare(request)
        async def emit(typ,**kw):
            data={'type':typ,**kw}
            await r.write(('event: '+typ+'\ndata: '+json.dumps(data)+'\n\n').encode())
        await emit('response.created',response={'id':rid,'object':'response','status':'in_progress'})
        if delay:await asyncio.sleep(delay)
        if isinstance(tool,dict):
            item={'type':'custom_tool_call','id':'ctc_'+uuid.uuid4().hex,'call_id':'call_'+uuid.uuid4().hex,'status':'completed',**tool}
            await emit('response.output_item.added',output_index=0,item={**item,'input':'','status':'in_progress'})
            await emit('response.custom_tool_call_input.delta',item_id=item['id'],output_index=0,delta=item['input'])
        elif tool:
            item={'type':'function_call','id':'fc_'+uuid.uuid4().hex,'call_id':'call_'+uuid.uuid4().hex,'name':tool[0],'arguments':json.dumps(tool[1]),'status':'completed'}
            await emit('response.output_item.added',output_index=0,item={**item,'arguments':'','status':'in_progress'})
            await emit('response.function_call_arguments.delta',item_id=item['id'],output_index=0,delta=item['arguments'])
        else:
            item={'type':'message','id':'msg_'+uuid.uuid4().hex,'role':'assistant','status':'completed','content':[{'type':'output_text','text':text,'annotations':[]}]}
            await emit('response.output_item.added',output_index=0,item={**item,'content':[],'status':'in_progress'})
            await emit('response.output_text.delta',item_id=item['id'],output_index=0,content_index=0,delta=text)
        await emit('response.output_item.done',output_index=0,item=item)
        await emit('response.completed',response={'id':rid,'object':'response','status':'completed','output':[item],'usage':{'input_tokens':100,'output_tokens':20,'input_tokens_details':{'cached_tokens':0},'total_tokens':120}})
        await r.write_eof();return r

def config(env,work,port,extra=''):
    cfg=f'''model = "gpt-5.6-sol"
model_provider = "mock"
approval_policy = "never"
sandbox_mode = "danger-full-access"
check_for_update_on_startup = false
web_search = "disabled"
cli_auth_credentials_store = "file"
[model_providers.mock]
name = "Isolated mock provider"
base_url = "http://127.0.0.1:{port}/v1"
wire_api = "responses"
requires_openai_auth = false
[analytics]
enabled = false
[feedback]
enabled = false
[features]
apps = false
hooks = false
[projects."{work}"]
trust_level = "trusted"
''' + extra
    (pathlib.Path(env['CODEX_HOME'])/'config.toml').write_text(cfg)

class RPC:
    def __init__(self,case,env,work):self.case=case;self.env=env;self.work=work;self.n=0;self.pending={};self.events=[]
    async def start(self):
        self.err=open(ROOT/self.case/'app-server.stderr.log','wb')
        self.p=await asyncio.create_subprocess_exec(BIN,'app-server','--stdio',env=self.env,cwd=self.work,stdin=asyncio.subprocess.PIPE,stdout=asyncio.subprocess.PIPE,stderr=self.err,start_new_session=True)
        self.reader=asyncio.create_task(self.read())
        await self.call('initialize',{'clientInfo':{'name':'cm_isolated_notification_test','version':'1'},'capabilities':{'experimentalApi':True}})
        await self.send({'method':'initialized'})
        return self
    async def send(self,m):self.p.stdin.write((json.dumps(m)+'\n').encode());await self.p.stdin.drain()
    async def read(self):
        while l:=await self.p.stdout.readline():
            try:m=json.loads(l)
            except ValueError:continue
            self.events.append({'at':time.time(),**m});write_json(ROOT/self.case/'rpc-events.json',self.events)
            if m.get('id') in self.pending and ('result' in m or 'error' in m):self.pending.pop(m['id']).set_result(m)
    async def call(self,method,params=None,timeout=15):
        self.n+=1;fid=self.n;f=asyncio.get_running_loop().create_future();self.pending[fid]=f
        await self.send({'id':fid,'method':method,'params':params or {}})
        r=await asyncio.wait_for(f,timeout)
        if 'error' in r:raise RuntimeError(json.dumps(r['error']))
        return r['result']
    async def wait_event(self,method,after=0,timeout=20,predicate=lambda x:True):
        end=time.monotonic()+timeout
        while time.monotonic()<end:
            for x in self.events[after:]:
                if x.get('method')==method and predicate(x):return x
            await asyncio.sleep(.05)
        raise TimeoutError(method)
    async def close(self):
        if self.p.returncode is None:
            self.p.stdin.close()
            try:await asyncio.wait_for(self.p.wait(),3)
            except asyncio.TimeoutError:
                os.killpg(self.p.pid,signal.SIGTERM)
                try:await asyncio.wait_for(self.p.wait(),3)
                except asyncio.TimeoutError:os.killpg(self.p.pid,signal.SIGKILL);await self.p.wait()
        await self.reader;self.err.close()

async def smoke():
    case='smoke';env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port)
    rpc=await RPC(case,env,work).start()
    try:
        thread=await rpc.call('thread/start',{'cwd':str(work),'model':'gpt-5.6-sol','modelProvider':'mock','approvalPolicy':'never','sandbox':'danger-full-access'})
        print('THREAD',thread['thread']['id'],flush=True)
        await rpc.call('turn/start',{'threadId':thread['thread']['id'],'input':[{'type':'text','text':'FIXTURE_SMOKE'}]})
        done=await rpc.wait_event('turn/completed');print('DONE',done,flush=True)
        print('REQUEST_COUNT',len(mock.requests),flush=True)
    finally:await rpc.close();await mock.close()
if __name__=='__main__':asyncio.run(smoke())

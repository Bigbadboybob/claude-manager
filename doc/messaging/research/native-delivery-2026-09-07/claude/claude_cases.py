from claude_harness import *
import socket

def main_requests(m):return [x for x in m.requests if x['body'].get('tools')]
def contains(m,text):return any(text in json.dumps(x['body'].get('messages')) for x in main_requests(m))
def mcp_args(root):return ['--mcp-config',json.dumps({'mcpServers':{'fixture':{'command':'/usr/bin/python3','args':[str(HERE/'claude_mcp.py'),str(root)],'timeout':60000}}})]
def save(case,data):
 (ROOT/case/'result.json').write_text(json.dumps(data,indent=2));print(case,json.dumps(data),flush=True)
def tool_reply(name,args):
 count=0
 def r(b,n):
  nonlocal count
  if not b.get('tools'):return {'text':'Fixture title'}
  count+=1
  return {'tool':(name,args)} if count==1 else {'text':'FIXTURE_IDLE_'+str(count)}
 return r

def background(kind):
 case='claude-'+kind;root=ROOT/case;root.mkdir(parents=True,exist_ok=True); release=root/'release';release.unlink(missing_ok=True)
 if kind=='bash':
  command=f"while test ! -f {release}; do sleep 0.1; done; cat {release}"
  r=tool_reply('Bash',{'command':command,'run_in_background':True,'description':'Wait for fixture notification'})
 else:r=tool_reply('mcp__fixture__wait',{})
 mock=Mock(case,r);client=Client(case,mock,extra=mcp_args(root),env_extra={'CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS':'700'})
 try:
  time.sleep(3);client.send('RUN_FIXTURE\r');settled=wait(lambda:len(main_requests(mock))>=2,15);time.sleep(1)
  client.send('UNSENT_DRAFT_'+kind.upper());time.sleep(.3)
  before=len(main_requests(mock));start=time.time();release.write_text('CM_NOTIFICATION_'+kind.upper());woke=wait(lambda:len(main_requests(mock))>before,12);time.sleep(.5)
  req=main_requests(mock)[-1] if main_requests(mock) else {}
  save(case,{'settled_before_release':settled,'requests_before':before,'requests_after':len(main_requests(mock)),'woke':woke,'latency_s':(req.get('at',start)-start) if woke else None,'draft_in_request':contains(mock,'UNSENT_DRAFT_'),'notification_in_request':contains(mock,'CM_NOTIFICATION_'),'task_notification':contains(mock,'task-notification')})
  client.send('\r');draft_intact=wait(lambda:contains(mock,'UNSENT_DRAFT_'+kind.upper()),5)
  data=json.loads((root/'result.json').read_text());data['draft_submitted_intact']=draft_intact;(root/'result.json').write_text(json.dumps(data,indent=2))
 finally:client.close();mock.close()

def hook(kind):
 case='claude-hook-'+kind;root=ROOT/case;root.mkdir(parents=True,exist_ok=True);(root/'release').unlink(missing_ok=True)
 cmd=f"while test ! -f {root}/release; do sleep 0.1; done; "
 cmd+=('echo CM_HOOK_REWAKE >&2; exit 2' if kind=='rewake' else "echo '{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"additionalContext\":\"CM_HOOK_ASYNC\"}}'")
 settings={'hooks':{'SessionStart':[{'hooks':[{'type':'command','command':cmd,'timeout':60,('asyncRewake' if kind=='rewake' else 'async'):True}]}]}}
 mock=Mock(case);client=Client(case,mock,settings=settings)
 try:
  time.sleep(3);client.send('BASELINE\r');wait(lambda:len(main_requests(mock))>=1);time.sleep(1);client.send('UNSENT_DRAFT_HOOK');time.sleep(.2)
  before=len(main_requests(mock));start=time.time();(root/'release').write_text('go');woke=wait(lambda:len(main_requests(mock))>before,6);time.sleep(.3)
  summary={'woke_idle':woke,'requests_before':before,'requests_after':len(main_requests(mock)),'latency_s':main_requests(mock)[-1]['at']-start if woke else None,'draft_in_request_before_submit':contains(mock,'UNSENT_DRAFT_HOOK'),'hook_context_before_submit':contains(mock,'CM_HOOK_')}
  client.send('\r');wait(lambda:len(main_requests(mock))>(before+int(woke)),5);summary['draft_submitted_intact']=contains(mock,'UNSENT_DRAFT_HOOK');summary['hook_context_after_submit']=contains(mock,'CM_HOOK_');save(case,summary)
 finally:client.close();mock.close()

def channel():
 case='claude-channel';root=ROOT/case;root.mkdir(parents=True,exist_ok=True)
 for p in root.glob('event-*.json'):p.unlink()
 mock=Mock(case);client=Client(case,mock,extra=[*mcp_args(root),'--dangerously-load-development-channels','server:fixture'])
 try:
  time.sleep(3);print('CHANNEL_SCREEN',client.buf.decode(errors='replace')[-2200:],flush=True)
  # Local fixture consent only; select development entry after inspecting screen.
  if b'I am using this for local development' in client.buf:client.send('\r');time.sleep(2)
  client.send('BASELINE\r');wait(lambda:len(main_requests(mock))>=1,8);time.sleep(1);client.send('UNSENT_DRAFT_CHANNEL');time.sleep(.2)
  before=len(main_requests(mock));start=time.time();(root/'event-1.json').write_text(json.dumps({'method':'notifications/claude/channel','params':{'content':'CM_CHANNEL_NOTIFICATION','meta':{'delivery_id':'fixture-1'}}}))
  woke=wait(lambda:len(main_requests(mock))>before,6);time.sleep(.2)
  save(case,{'woke':woke,'requests_before':before,'requests_after':len(main_requests(mock)),'latency_s':main_requests(mock)[-1]['at']-start if woke else None,'draft_in_request':contains(mock,'UNSENT_DRAFT_CHANNEL'),'notification_in_request':contains(mock,'CM_CHANNEL_NOTIFICATION')})
  print('CHANNEL_END',client.buf.decode(errors='replace')[-2200:],flush=True)
 finally:client.close();mock.close()
if __name__=='__main__':
 for x in sys.argv[1:]:
  if x in ['bash','mcp']:background(x)
  elif x in ['async','rewake']:hook(x)
  elif x=='channel':channel()

from claude_cases import *

def run(kind,mode):
 case=f'claude-{kind}-{mode}';root=ROOT/case;root.mkdir(parents=True,exist_ok=True)
 for name in ['release','active_release']:(root/name).unlink(missing_ok=True)
 count=0
 def r(b,n):
  nonlocal count
  if not b.get('tools'):return {'text':'Fixture'}
  count+=1
  if count==1:
   if kind=='mcp':return {'tool':('mcp__fixture__wait',{})}
   return {'tool':('Bash',{'command':f'while test ! -f {root}/release; do sleep 0.1; done; cat {root}/release','run_in_background':True,'description':'Notification wait fixture'})}
  if count==2 and mode in ['active','approval']:
   cmd=f'while test ! -f {root}/active_release; do sleep 0.1; done; echo ACTIVE_FINISHED' if mode=='active' else 'printf CM_PERMISSION_FIXTURE'
   return {'tool':('Bash',{'command':cmd,'description':'Foreground checkpoint fixture'})}
  return {'text':f'DONE_{count}'}
 settings={'permissions':{'defaultMode':'default','allow':['mcp__fixture__wait'],'ask':['Bash']}} if mode=='approval' else None
 extra=mcp_args(root)
 if mode=='timeout':extra=['--mcp-config',json.dumps({'mcpServers':{'fixture':{'command':'/usr/bin/python3','args':[str(HERE/'claude_mcp.py'),str(root)],'timeout':2500}}})]
 mock=Mock(case,r);client=Client(case,mock,extra=extra,settings=settings,env_extra={'CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS':'400'})
 try:
  time.sleep(3);client.send('RUN_FIXTURE\r');assert wait(lambda:len(main_requests(mock))>=2,8);time.sleep(.5)
  before=len(main_requests(mock));start=time.time()
  if mode!='timeout':(root/'release').write_text('CM_EDGE_NOTIFICATION')
  woke=wait(lambda:len(main_requests(mock))>before,4)
  data={'requests_before':before,'woke_while_foreground_blocked':woke,'notification_before_release':contains(mock,'CM_EDGE_NOTIFICATION'),'task_notification_before_release':contains(mock,'<task-notification>')}
  if mode=='active':(root/'active_release').write_text('go')
  if mode=='approval':
   import re
   plain=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',client.buf.decode(errors='replace'))
   data['approval_was_visible']='Doyouwanttoproceed?' in ''.join(plain.split());client.send('\r')
  if mode in ['active','approval']:wait(lambda:len(main_requests(mock))>before,6)
  data['notification_after_release']=contains(mock,'CM_EDGE_NOTIFICATION');data['task_notification_after_release']=contains(mock,'<task-notification>')
  if mode=='timeout':
   messages=[x['body'].get('messages') for x in main_requests(mock)];data['timeout_context']=any('timed out' in json.dumps(m).lower() or 'timeout' in json.dumps(m).lower() for m in messages)
  save(case,data)
 finally:client.close();mock.close()
if __name__=='__main__':
 for k,m in [('mcp','active'),('bash','active'),('mcp','approval'),('mcp','timeout')]:run(k,m)

from claude_cases import *
import re

def endpoint(client):
 p=client.root/'debug.log'
 def get():
  found=re.findall(r'\[uds-messaging\] Listening: (.+)',p.read_text() if p.exists() else '')
  return found[-1] if found else None
 assert wait(get,8),'socket did not start'
 return get()
def post(ep,content,msgid=None):
 s=socket.socket(socket.AF_UNIX);s.connect(ep);s.sendall((json.dumps({'type':'user','from':'cm-fixture','message':{'role':'user','content':content},**({'msg_id':msgid} if msgid else {})})+'\n').encode());s.close()
def run(mode):
 case='claude-socket-'+mode;root=ROOT/case;root.mkdir(parents=True,exist_ok=True);release=root/'release';release.unlink(missing_ok=True)
 settings={'crossSessionInbound':'accept'};extra=[]
 if mode=='default':settings={}
 if mode=='refuse':settings={'crossSessionInbound':'refuse'}
 if mode=='ownchild':r=tool_reply('mcp__fixture__post_self',{'text':'CM_SELF_SOCKET_EVENT'});settings={};extra=mcp_args(root)
 elif mode=='active':r=tool_reply('Bash',{'command':f'while test ! -f {release}; do sleep 0.1; done; echo ACTIVE_TOOL_DONE','description':'Fixture controlled active tool'})
 elif mode=='approval':
  r=tool_reply('Bash',{'command':'printf CM_APPROVAL_TEST','description':'Harmless fixture requiring approval'});settings['permissions']={'defaultMode':'default','ask':['Bash']}
 else:r=None
 mock=Mock(case,r);client=Client(case,mock,settings=settings,extra=extra)
 try:
  ep=endpoint(client);time.sleep(2);client.send('BASELINE\r');assert wait(lambda:len(main_requests(mock))>=1)
  time.sleep(2);before=len(main_requests(mock))
  if mode=='ownchild':
   time.sleep(2);save(case,{'self_event_received':contains(mock,'CM_SELF_SOCKET_EVENT'),'calls':before,'mcp_events':(root/'mcp-events.jsonl').read_text()});return
  if mode not in ['active','approval']:client.send('UNSENT_DRAFT_SOCKET');time.sleep(.2)
  start=time.time();post(ep,'CM_SOCKET_EVENT','socket-fixture-1')
  woke=wait(lambda:len(main_requests(mock))>before,3)
  data={'woke_before_release':woke,'latency_s':main_requests(mock)[-1]['at']-start if woke else None,'draft_in_request_before_submit':contains(mock,'UNSENT_DRAFT_SOCKET'),'notification_in_request':contains(mock,'CM_SOCKET_EVENT')}
  if mode=='active':
   release.write_text('go');wait(lambda:len(main_requests(mock))>before,6);data['received_after_tool_completion']=contains(mock,'CM_SOCKET_EVENT')
  elif mode=='approval':
   plain=re.sub(r'\x1b\[[0-9;?]*[A-Za-z]','',client.buf.decode(errors='replace'))
   data['approval_visible']='Doyouwanttoproceed?' in ''.join(plain.split())
   client.send('\r');wait(lambda:len(main_requests(mock))>before,8);data['received_after_approval']=contains(mock,'CM_SOCKET_EVENT')
  elif mode=='idle':
   after=len(main_requests(mock));post(ep,'CM_SOCKET_EVENT','socket-fixture-1');time.sleep(1);data['same_id_same_body_extra_requests']=len(main_requests(mock))-after
   post(ep,'CM_SOCKET_CHANGED_SAME_ID','socket-fixture-1');time.sleep(1);data['changed_body_same_id_received']=contains(mock,'CM_SOCKET_CHANGED_SAME_ID')
   post(ep,'CM_SOCKET_RECONNECT','socket-fixture-2');wait(lambda:contains(mock,'CM_SOCKET_RECONNECT'),3);data['new_connection_received']=contains(mock,'CM_SOCKET_RECONNECT')
   client.send('\r');wait(lambda:contains(mock,'UNSENT_DRAFT_SOCKET'),5);data['draft_submitted_intact']=contains(mock,'UNSENT_DRAFT_SOCKET')
  save(case,data)
 finally:client.close();mock.close()
if __name__=='__main__':
 for x in sys.argv[1:]:run(x)

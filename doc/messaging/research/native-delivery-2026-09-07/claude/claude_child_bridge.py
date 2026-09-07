from claude_socket_cases import *
case='claude-child-bridge';root=ROOT/case;root.mkdir(parents=True,exist_ok=True)
for p in root.glob('event-*.json'):p.unlink()
mock=Mock(case);client=Client(case,mock,extra=mcp_args(root))
try:
 ep=endpoint(client);time.sleep(2);client.send('BASELINE\r');wait(lambda:len(main_requests(mock))>=1);time.sleep(1);client.send('UNSENT_DRAFT_CHILD');time.sleep(.2)
 data=[]
 for i in range(2):
  start=time.time();(root/f'event-{i}.json').write_text(json.dumps({'method':'fixture/socket','params':{'content':f'CM_CHILD_EVENT_{i}'}}));got=wait(lambda:contains(mock,f'CM_CHILD_EVENT_{i}'),4);data.append({'event':i,'received':got,'latency_s':main_requests(mock)[-1]['at']-start if got else None});time.sleep(.5)
 before=contains(mock,'UNSENT_DRAFT_CHILD');client.send('\r');wait(lambda:contains(mock,'UNSENT_DRAFT_CHILD'),4)
 save(case,{'events':data,'draft_not_submitted':not before,'draft_intact':contains(mock,'UNSENT_DRAFT_CHILD'),'explicit_inbound_setting':False,'arming_tool_calls':False})
finally:client.close();mock.close()

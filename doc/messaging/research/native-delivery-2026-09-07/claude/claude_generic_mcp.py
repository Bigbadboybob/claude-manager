from claude_cases import *
case='claude-generic-mcp';root=ROOT/case;root.mkdir(parents=True,exist_ok=True)
for p in root.glob('event-*.json'):p.unlink()
mock=Mock(case);client=Client(case,mock,extra=mcp_args(root))
try:
 time.sleep(3);client.send('BASELINE\r');wait(lambda:len(main_requests(mock))>=1);time.sleep(1);before=len(main_requests(mock))
 events=[{'method':'notifications/message','params':{'level':'info','logger':'cm-fixture','data':'GENERIC_LOG_EVENT'}},{'method':'notifications/resources/updated','params':{'uri':'fixture://notifications'}},{'method':'notifications/tools/list_changed'}, {'method':'notifications/progress','params':{'progressToken':'fixture','progress':1,'total':1,'message':'GENERIC_PROGRESS_EVENT'}},{'method':'notifications/tasks/status','params':{'taskId':'fixture','status':'completed','createdAt':'2026-09-07T18:00:00Z','lastUpdatedAt':'2026-09-07T18:00:01Z','ttl':60000}},{'id':'server-sampling-1','method':'sampling/createMessage','params':{'messages':[{'role':'user','content':{'type':'text','text':'SAMPLING_EVENT'}}],'maxTokens':10}}]
 for i,e in enumerate(events):(root/f'event-{i}.json').write_text(json.dumps(e));time.sleep(.2)
 time.sleep(3);data={'requests_before':before,'requests_after':len(main_requests(mock)),'woke_idle':len(main_requests(mock))>before}
 client.send('AFTER_GENERIC\r');wait(lambda:len(main_requests(mock))>before);data['log_or_progress_in_next_request']=contains(mock,'GENERIC_LOG_EVENT') or contains(mock,'GENERIC_PROGRESS_EVENT');log=[json.loads(x) for x in (root/'mcp-events.jsonl').read_text().splitlines()];data['initialize']=[x['initialize'] for x in log if 'initialize' in x];data['sampling_reply']=[x['reply'] for x in log if 'reply' in x];save(case,data)
finally:client.close();mock.close()

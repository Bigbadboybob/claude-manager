from claude_cases import *
case='claude-stream-owned';mock=Mock(case);client=Client(case,mock,interactive=False)
try:
 client.send('STREAM_BASELINE');first=wait(lambda:any(e.get('type')=='result' for e in client.events),12);time.sleep(.5)
 running=client.proc.poll() is None;before=len(main_requests(mock));start=time.time();client.send('STREAM_SECOND_NATIVE_MESSAGE');second=wait(lambda:contains(mock,'STREAM_SECOND_NATIVE_MESSAGE'),5)
 save(case,{'first_result':first,'alive_with_stdin_open':running,'second_turn_same_process':second,'latency_s':main_requests(mock)[-1]['at']-start if second else None,'session_ids':list(set(e.get('session_id') for e in client.events if e.get('session_id'))),'requests_before':before,'requests_after':len(main_requests(mock))})
finally:client.close();mock.close()

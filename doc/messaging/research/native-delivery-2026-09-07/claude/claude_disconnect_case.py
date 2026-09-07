from claude_cases import *
case='claude-mcp-disconnect';root=ROOT/case;root.mkdir(parents=True,exist_ok=True)
for p in root.glob('event-*.json'):p.unlink()
(root/'release').unlink(missing_ok=True)
mock=Mock(case,tool_reply('mcp__fixture__wait',{}));client=Client(case,mock,extra=mcp_args(root),env_extra={'CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS':'400'})
try:
 time.sleep(3);client.send('RUN_FIXTURE\r');assert wait(lambda:len(main_requests(mock))>=2,8);time.sleep(.4);before=len(main_requests(mock));(root/'event-exit.json').write_text(json.dumps({'method':'fixture/exit'}));woke=wait(lambda:len(main_requests(mock))>before,5)
 messages=json.dumps([r['body'].get('messages') for r in main_requests(mock)]).lower()
 save(case,{'completion_after_server_disconnect':woke,'task_notification':contains(mock,'<task-notification>'),'connection_error_in_context':'connection closed' in messages or 'connection lost' in messages,'result_is_notification_payload':contains(mock,'CM_NOTIFICATION_MCP')})
finally:client.close();mock.close()

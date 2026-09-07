import json,time,sys,pathlib
m=json.load(sys.stdin)
time.sleep(3)
marker='HOOK_NOTIFICATION_'+m.get('hook_event_name','UNKNOWN')
pathlib.Path(sys.argv[1]).write_text(marker)
print(json.dumps({'hookSpecificOutput':{'hookEventName':m['hook_event_name'],'additionalContext':marker}}))

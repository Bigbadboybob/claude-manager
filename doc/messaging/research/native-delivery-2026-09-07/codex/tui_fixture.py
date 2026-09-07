from harness import *
import pty,fcntl,termios,struct
if os.environ.get('CM_CODEX_RESEARCH_PYTE_PATH'): sys.path.insert(0,os.environ['CM_CODEX_RESEARCH_PYTE_PATH'])
import pyte

class TUI:
 def __init__(self,case,env,work,args=()):self.case=case;self.env=env;self.work=work;self.args=args;self.raw=b'';self.screen=pyte.Screen(120,35);self.stream=pyte.ByteStream(self.screen)
 async def start(self):
  self.master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',35,120,0,0))
  def setup():os.setsid();fcntl.ioctl(slave,termios.TIOCSCTTY,0)
  self.p=await asyncio.create_subprocess_exec(BIN,*self.args,'--no-alt-screen',env=self.env,cwd=self.work,stdin=slave,stdout=slave,stderr=slave,preexec_fn=setup)
  os.close(slave);os.set_blocking(self.master,False)
  self.reader=asyncio.create_task(self.read());return self
 async def read(self):
  while self.p.returncode is None:
   try:
    data=os.read(self.master,65536)
    if not data:break
    self.raw+=data
    (ROOT/self.case/'tui.raw').write_bytes(self.raw)
    self.stream.feed(data)
    for q,r in [(b'\x1b[6n',b'\x1b[1;1R'),(b'\x1b]10;?',b'\x1b]10;rgb:dddd/dddd/dddd\x1b\\'),(b'\x1b]11;?',b'\x1b]11;rgb:1111/1111/1111\x1b\\'),(b'\x1b[?u',b'\x1b[?0u'),(b'\x1b[c',b'\x1b[?1;2c')]:
     if q in data:os.write(self.master,r)
   except BlockingIOError:pass
   except OSError:break
   await asyncio.sleep(.01)
 def visible(self):return '\n'.join(self.screen.display)
 def snapshot(self,name):p=ROOT/self.case/(name+'.txt');p.write_text(self.visible());return self.visible()
 async def send(self,b,delay=.15):os.write(self.master,b.encode() if isinstance(b,str) else b);await asyncio.sleep(delay)
 async def wait_text(self,text,timeout=15):
  end=time.monotonic()+timeout
  while time.monotonic()<end:
   if text in self.visible():return
   if self.p.returncode is not None:raise RuntimeError('TUI exited '+str(self.p.returncode)+' '+self.visible())
   await asyncio.sleep(.05)
  raise TimeoutError(text+' '+self.visible())
 async def close(self):
  if self.p.returncode is None:
   os.killpg(self.p.pid,signal.SIGTERM)
   try:await asyncio.wait_for(self.p.wait(),3)
   except asyncio.TimeoutError:os.killpg(self.p.pid,signal.SIGKILL);await self.p.wait()
  await self.reader;os.close(self.master)

async def main():
 case='queue-tui';env,work=make_env(case);mock=await Mock(case).start();config(env,work,mock.port)
 tui=await TUI(case,env,work).start();result={}
 try:
  await asyncio.sleep(2);print(tui.snapshot('initial'),flush=True)
  await tui.send('SETUP');await tui.send('\r');await tui.wait_text('MOCK_RESPONSE_1')
  import sqlite3
  c=sqlite3.connect('file:'+env['CODEX_HOME']+'/state_5.sqlite?mode=ro',uri=True)
  tid=c.execute('select id from threads order by created_at desc limit 1').fetchone()[0];c.close();result['threadId']=tid
  await tui.send('HUMAN_DRAFT_DO_NOT_SEND');result['draft_before']=tui.snapshot('draft-before')
  p=await asyncio.create_subprocess_exec(BIN,'queue','--thread',tid,'--message','QUEUE_IDLE_FROM_EXTERNAL',env=env,cwd=work,stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE)
  out,err=await asyncio.wait_for(p.communicate(),10);result['queue_cli']={'code':p.returncode,'stdout':out.decode(),'stderr':err.decode()}
  await asyncio.sleep(15);result['requests_after_15s']=mock.responses;result['draft_after']=tui.snapshot('draft-after-queue')
  await tui.send('\r');await asyncio.sleep(4);result['requests_after_human_submit']=mock.responses;result['after_human_submit']=tui.snapshot('after-human-submit')
  write_json(ROOT/case/'results.json',result);print(json.dumps(result,indent=2),flush=True)
 finally:await tui.close();await mock.close()
if __name__=='__main__':asyncio.run(main())

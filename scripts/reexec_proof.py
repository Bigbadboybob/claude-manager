#!/usr/bin/env python3
"""Proof of the re-exec restart mechanism's core OS assumptions.

Stage 1 ("old daemon"): spawn a bash child on a PTY, run a command,
deliberately LEAVE its output unread in the kernel PTY buffer, clear
CLOEXEC on the master fd, then exec() this same script ("new binary").

Stage 2 ("new daemon", same PID): verify
  1. the child survived the exec and is still parented to us (same pid),
  2. a pidfd can be re-acquired from the stored numeric pid,
  3. bytes written PRE-exec are still in the PTY buffer (nothing lost),
  4. the PTY still flows POST-exec (write a new command, read its output).
"""
import os, sys, time

if "--after" in sys.argv:
    master, child = int(sys.argv[2]), int(sys.argv[3])
    me = os.getpid()
    ppid_of_child = int(
        [l for l in open(f"/proc/{child}/status") if l.startswith("PPid:")][0].split()[1]
    )
    os.kill(child, 0)  # raises if child is gone
    pidfd = os.pidfd_open(child)
    os.write(master, b"echo POST-EXEC-$((21*2))\n")
    time.sleep(0.6)
    out = os.read(master, 65536).decode(errors="replace")
    pre_ok = "PRE-EXEC-7" in out
    post_ok = "POST-EXEC-42" in out
    print(f"new-image pid {me}: child {child} alive, ppid={ppid_of_child} "
          f"({'still our child' if ppid_of_child == me else 'REPARENTED - FAIL'})")
    print(f"pidfd re-acquired: fd {pidfd}")
    print(f"pre-exec bytes survived in PTY buffer: {pre_ok}")
    print(f"PTY flows post-exec: {post_ok}")
    os.write(master, b"exit\n")
    time.sleep(0.2)
    sys.exit(0 if (pre_ok and post_ok and ppid_of_child == me) else 1)

master, slave = os.openpty()
pid = os.fork()
if pid == 0:
    os.dup2(slave, 0), os.dup2(slave, 1), os.dup2(slave, 2)
    os.close(master), os.close(slave)
    os.execvp("bash", ["bash", "--norc"])
os.close(slave)
time.sleep(0.3)
os.write(master, b"echo PRE-EXEC-$((3+4))\n")
time.sleep(0.4)          # let bash produce output; do NOT read it
os.set_inheritable(master, True)  # clear CLOEXEC: fd must survive exec
print(f"old-image pid {os.getpid()}: child {pid} on master fd {master}; "
      f"exec'ing new image with unread bytes pending...")
os.execv(sys.executable, [sys.executable, sys.argv[0], "--after", str(master), str(pid)])

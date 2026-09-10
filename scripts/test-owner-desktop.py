#!/usr/bin/python3
"""Exercise the real notify-rust → D-Bus transport against a private receiver.

Run within scripts/cm-test-isolated, which keeps live desktop/CM state isolated:
  scripts/cm-test-isolated dbus-run-session -- /usr/bin/python3 scripts/test-owner-desktop.py
Requires system python dbus + gi. This is transport verification, not evidence
that Owner saw a notification on the laptop.
"""
import os
import subprocess
import sys

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

DBusGMainLoop(set_as_default=True)
loop = GLib.MainLoop()
calls = []


class Notifications(dbus.service.Object):
    @dbus.service.method("org.freedesktop.Notifications", in_signature="", out_signature="as")
    def GetCapabilities(self):
        return ["body"]

    @dbus.service.method("org.freedesktop.Notifications", in_signature="", out_signature="ssss")
    def GetServerInformation(self):
        return ("CM test receiver", "CM", "1", "1.2")

    @dbus.service.method("org.freedesktop.Notifications", in_signature="susssasa{sv}i", out_signature="u")
    def Notify(self, app, replaces, icon, summary, body, actions, hints, timeout):
        calls.append((str(summary), str(body)))
        return 1


bus = dbus.SessionBus()
name = dbus.service.BusName("org.freedesktop.Notifications", bus)
receiver = Notifications(bus, "/org/freedesktop/Notifications")
env = dict(os.environ, CM_TEST_OWNER_DESKTOP="1")
child = subprocess.Popen([
    "cargo", "test", "-p", "claude-manager-tui",
    "owner_notification::tests::owner_notification_desktop_transport", "--",
    "--ignored", "--exact", "--nocapture",
], env=env)


def poll():
    if child.poll() is None:
        return True
    loop.quit()
    return False


GLib.timeout_add(100, poll)
loop.run()
assert child.returncode == 0, child.returncode
assert calls == [("Claude Manager", "Task: Review ready")], calls
print("Desktop D-Bus accepted the alert; reconnect submitted no duplicate.")

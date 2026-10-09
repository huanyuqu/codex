"""Run a CLI in a controlling terminal and forward input/output as JSONL."""

import base64
import errno
import fcntl
import json
import os
import pty
import select
import signal
import struct
import sys
import termios

pid, master = pty.fork()
if pid == 0:
    os.execvpe(sys.argv[1], sys.argv[1:], os.environ)
fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 140, 0, 0))
pending = b""
probe = b""
def emit(message):
    print(json.dumps(message), flush=True)

try:
    while True:
        readable, _, _ = select.select([master, sys.stdin], [], [], 0.1)
        if master in readable:
            try:
                data = os.read(master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not data:
                break
            emit({"type": "output", "data": base64.b64encode(data).decode()})
            probe += data
            probes = [
                (b"\x1b[6n", b"\x1b[1;1R"),
                (b"\x1b[?u", b"\x1b[?0u"),
                (b"\x1b[c", b"\x1b[?62;4;6;22c"),
                (b"\x1b]10;?\x07", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
                (b"\x1b]11;?\x07", b"\x1b]11;rgb:0000/0000/0000\x1b\\"),
            ]
            for request, response in probes:
                while request in probe:
                    os.write(master, response)
                    probe = probe.replace(request, b"", 1)
            probe = probe[-128:]
        if sys.stdin in readable:
            data = os.read(sys.stdin.fileno(), 65536)
            if not data:
                break
            pending += data
            while b"\n" in pending:
                line, pending = pending.split(b"\n", 1)
                command = json.loads(line)
                if command.get("type") == "input":
                    os.write(master, base64.b64decode(command["data"]))
                elif command.get("type") == "stop":
                    os.kill(pid, signal.SIGTERM)
        waited, status = os.waitpid(pid, os.WNOHANG)
        if waited:
            emit({"type": "exit", "status": status})
            break
finally:
    os.close(master)
    try:
        os.killpg(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass

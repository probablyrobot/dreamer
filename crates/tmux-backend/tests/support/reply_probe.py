"""Send terminal queries from inside a pane and report every reply.

usage: python3 reply_probe.py QUERY_HEX [QUERY_HEX ...]

For each query: write it, collect everything the terminal sends back for
one second, then print `REPLY <index> <hex>` on its own line. Ends with
`PROBE-DONE`. Synthetic test helper for tests/replies.rs.
"""
import os
import select
import sys
import termios
import tty

fd = sys.stdin.fileno()
saved = termios.tcgetattr(fd)
tty.setraw(fd)
os.read(fd, 1)  # wait for the go key
results = []
try:
    for query in sys.argv[1:]:
        os.write(sys.stdout.fileno(), bytes.fromhex(query))
        got = b""
        while True:
            ready, _, _ = select.select([fd], [], [], 1.0)
            if not ready:
                break
            chunk = os.read(fd, 4096)
            if not chunk:
                break
            got += chunk
        results.append(got)
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, saved)
for i, got in enumerate(results):
    sys.stdout.write("REPLY %d %s\r\n" % (i, got.hex() or "-"))
sys.stdout.write("PROBE-DONE\r\n")
sys.stdout.flush()
os.system("sleep 60")

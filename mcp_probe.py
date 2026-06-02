#!/usr/bin/env python3
import json, subprocess, sys
BIN = "/home/okhsunrog/code/rust/flashprobe-mcp/target/release/flashprobe-mcp"
def main():
    tool, args = sys.argv[1], json.loads(sys.argv[2]) if len(sys.argv) > 2 else {}
    p = subprocess.Popen([BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, text=True, bufsize=1)
    def send(o): p.stdin.write(json.dumps(o) + "\n"); p.stdin.flush()
    def wait(i):
        for line in p.stdout:
            line = line.strip()
            if line and json.loads(line).get("id") == i: return json.loads(line)
    send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}})
    wait(1); send({"jsonrpc":"2.0","method":"notifications/initialized"})
    send({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":args}})
    r = wait(2); p.terminate()
    if "error" in r: print("ERROR:", json.dumps(r["error"], indent=2)); return
    for c in r["result"]["content"]: print(c.get("text", c))
main()

"""Minimal real stdio MCP for gateway integration tests; Python standard library only."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

if sys.argv[1:2] == ["--no-handshake"]:
    time.sleep(30)

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "initialize":
        result = {"protocolVersion": "2025-03-26", "capabilities": {"tools": {}}, "serverInfo": {"name": "fixture", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "fixture echo", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        arguments = request["params"].get("arguments", {})
        if arguments.get("crash"):
            os._exit(7)
        if arguments.get("hang"):
            time.sleep(30)
        extra = {}
        if arguments.get("child"):
            extra["child_pid"] = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"]).pid
        result = {"content": [{"type": "text", "text": json.dumps({"arguments": arguments, "pid": os.getpid(), "cwd": str(Path.cwd()), "startup_args": sys.argv[1:], "literal": os.environ.get("FIXTURE_TOKEN"), "reference": os.environ.get("FIXTURE_REF"), "env_names": sorted(os.environ), **extra})}]}
    elif method == "ping":
        result = {}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32601, "message": "Unknown method"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)

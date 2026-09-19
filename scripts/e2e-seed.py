#!/usr/bin/env python3
"""Drives a *running* dev_server (started with HOME pointing at a scratch dir,
see scripts/e2e-run.sh) by replaying real-shaped Claude Code hook events over
its Unix socket and writing fixture transcripts where Claude Code would.

    e2e-seed.py start   <id> <cwd> "<prompt>" [--tokens N] [--ctx N]
    e2e-seed.py stop    <id>          # assistant finished a turn  -> Done
    e2e-seed.py work    <id>          # tool activity              -> Working
    e2e-seed.py permission <id>       # PermissionRequest hook     -> Waiting
    e2e-seed.py ask     <id>          # AskUserQuestion pre-tool   -> Waiting
    e2e-seed.py end     <id>          # SessionEnd                 -> Done (durable)
    e2e-seed.py usage   <id> <output_tokens> <ctx_used>   # append an assistant turn, then refresh via stop-less path
    e2e-seed.py sub     <id> <name> "<desc>" "<prompt>"   # subagent transcript
    e2e-seed.py plan    <id> "<subject>:done|todo" ...    # ~/.claude/tasks/<id>/N.json
"""
import json, os, socket, sys, time

HOME = os.environ.get("HOME")
SOCK = os.path.join(HOME, "Library", "Application Support", "SessionBoard", "engine.sock")
PROJECTS = os.path.join(HOME, ".claude", "projects")
TASKS = os.path.join(HOME, ".claude", "tasks")
META = os.path.join(os.path.dirname(SOCK), "e2e-meta.json")  # id -> cwd, so later commands can find the transcript


def meta():
    try:
        return json.load(open(META))
    except Exception:
        return {}


def save_meta(m):
    os.makedirs(os.path.dirname(META), exist_ok=True)
    json.dump(m, open(META, "w"))


def tpath(cwd, sid):
    return os.path.join(PROJECTS, cwd.replace("/", "-"), f"{sid}.jsonl")


def send(event, payload):
    s = socket.socket(socket.AF_UNIX)
    s.connect(SOCK)
    s.sendall(json.dumps({"event": event, "payload": payload}).encode())
    s.close()


def append(path, obj):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "a") as f:
        f.write(json.dumps(obj) + "\n")


def assistant(text, out=1200, ctx=30000, model="claude-haiku-4-5"):
    return {"type": "assistant", "message": {"role": "assistant", "model": model,
            "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": 100, "output_tokens": out, "cache_creation_input_tokens": 500,
                      "cache_read_input_tokens": max(0, ctx - 600)}}}


def main():
    a = sys.argv[1:]
    cmd = a[0]
    m = meta()
    if cmd == "start":
        sid, cwd, prompt = a[1], a[2], a[3]
        tokens = int(a[a.index("--tokens") + 1]) if "--tokens" in a else 1200
        ctx = int(a[a.index("--ctx") + 1]) if "--ctx" in a else 30000
        p = tpath(cwd, sid)
        append(p, {"type": "user", "message": {"role": "user", "content": prompt}})
        append(p, assistant("On it — " + prompt[:60], tokens, ctx))
        m[sid] = cwd
        save_meta(m)
        ep = a[a.index("--entrypoint") + 1] if "--entrypoint" in a else None
        payload = {"session_id": sid, "cwd": cwd, "transcript_path": p, "hook_event_name": "SessionStart"}
        if ep:
            payload["entrypoint"] = ep
        send("session-start", payload)
        return
    sid = a[1]
    cwd = m.get(sid, "")
    p = tpath(cwd, sid)
    base = {"session_id": sid, "cwd": cwd, "transcript_path": p}
    if cmd == "stop":
        send("stop", {**base, "hook_event_name": "Stop"})
    elif cmd == "work":
        send("pre-tool-use", {**base, "tool_name": "Bash", "tool_input": {}, "tool_use_id": f"tu{time.time_ns()}"})
    elif cmd == "permission":
        send("permission-request", {**base, "tool_name": "Bash", "tool_input": {}, "tool_use_id": f"tu{time.time_ns()}"})
    elif cmd == "ask":
        send("pre-tool-use", {**base, "tool_name": "AskUserQuestion", "tool_input": {}, "tool_use_id": f"tu{time.time_ns()}"})
    elif cmd == "end":
        send("session-end", {**base, "hook_event_name": "SessionEnd"})
    elif cmd == "usage":
        append(p, assistant("more work", int(a[2]), int(a[3])))
        send("stop", {**base, "hook_event_name": "Stop"})
    elif cmd == "sub":
        name, desc, prompt = a[2], a[3], a[4]
        d = os.path.join(PROJECTS, cwd.replace("/", "-"), sid, "subagents")
        os.makedirs(d, exist_ok=True)
        json.dump({"description": desc}, open(os.path.join(d, f"agent-{name}.meta.json"), "w"))
        append(os.path.join(d, f"agent-{name}.jsonl"), {"type": "user", "message": {"role": "user", "content": prompt}})
        append(os.path.join(d, f"agent-{name}.jsonl"), assistant("subagent working", 800, 12000))
        send("stop", {**base, "hook_event_name": "Stop"})
    elif cmd == "plan":
        d = os.path.join(TASKS, sid)
        os.makedirs(d, exist_ok=True)
        for i, spec in enumerate(a[2:], 1):
            subject, status = spec.rsplit(":", 1)
            json.dump({"id": str(i), "subject": subject, "status": "completed" if status == "done" else "pending"},
                      open(os.path.join(d, f"{i}.json"), "w"))
        send("stop", {**base, "hook_event_name": "Stop"})
    else:
        sys.exit(__doc__)


main()

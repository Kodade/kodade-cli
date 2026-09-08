#!/usr/bin/env python3
import json, os, pathlib, shutil, signal, subprocess, sys, tempfile, time

binary = pathlib.Path(sys.argv[1]).resolve() if len(sys.argv) == 2 else sys.exit("usage: native-resume-smoke.py PATH")
root = pathlib.Path(tempfile.mkdtemp(prefix="kodade-native-resume-")); procs = []
try:
    home, runtime, state, bindir, work = (root / n for n in ("home", "runtime", "state", "bin", "work"))
    for path in (home / ".config/kodade-cli", runtime, state, bindir, work): path.mkdir(parents=True, exist_ok=True)
    (home / ".config/kodade-cli/config.toml").write_text("[session]\nresume_agents = true\n")
    log = root / "argv.jsonl"; fake = bindir / "codex"
    fake.write_text("#!/usr/bin/env python3\nimport json,os,sys,time\nopen(os.environ['ARGS_LOG'],'a').write(json.dumps({'pid':os.getpid(),'argv':sys.argv[1:]})+'\\n')\ntime.sleep(60)\n"); fake.chmod(0o700)
    env = {k:v for k,v in os.environ.items() if not k.startswith("KODADE_")}; env.update({"HOME":str(home),"XDG_RUNTIME_DIR":str(runtime),"XDG_STATE_HOME":str(state),"PATH":f"{bindir}:{env['PATH']}","ARGS_LOG":str(log)})
    def run(*args, input=None): return subprocess.run([binary,*args],env=env,cwd=work,input=input,text=True,capture_output=True,check=True,timeout=10)
    def start():
        proc=subprocess.Popen([binary,"daemon","native-smoke"],env=env,cwd=work,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); procs.append(proc); socket=runtime/"kodade-cli/native-smoke.sock"; end=time.monotonic()+5
        while not socket.exists():
            if proc.poll() is not None or time.monotonic()>end: raise RuntimeError("daemon did not bind")
            time.sleep(.02)
        return proc
    def stop(proc):
        proc.send_signal(signal.SIGTERM)
        try: proc.wait(3)
        except subprocess.TimeoutExpired: proc.kill(); proc.wait(3)
    hook = json.loads(run("integrate", "codex").stdout)["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
    daemon=start(); panes=[run("-s","native-smoke","run","--","codex").stdout.strip() for _ in range(2)]
    for pane, session in zip(panes,("first","second")):
        hook_env = dict(env, KODADE_BIN=str(binary), KODADE_PANE=pane, KODADE_SOCKET=str(runtime / "kodade-cli/native-smoke.sock"))
        subprocess.run(["sh", "-c", hook], env=hook_env, input=json.dumps({"session_id":session,"nested":{"session_id":"decoy"}}), text=True, check=True, timeout=10)
    time.sleep(.7); stop(daemon); log.write_text("")
    for count in (2,4):
        daemon=start(); end=time.monotonic()+5
        while len(log.read_text().splitlines())<count:
            if time.monotonic()>end: raise RuntimeError("resumed agents did not launch")
            time.sleep(.02)
        stop(daemon)
    records=[json.loads(line) for line in log.read_text().splitlines()]
    assert all(sorted(r["argv"] for r in records[offset:offset+2]) == [["resume","first"],["resume","second"]] for offset in (0,2)), records
    assert len({r["pid"] for r in records}) == 4, records
    print("native resume smoke passed")
finally:
    for proc in procs:
        if proc.poll() is None: proc.kill(); proc.wait()
    shutil.rmtree(root,ignore_errors=True)

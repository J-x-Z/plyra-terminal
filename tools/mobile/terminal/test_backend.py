#!/usr/bin/env python3
"""Real local PTY integration tests. Never connects to devices or uses user data."""
import argparse, base64, hashlib, json, os, pathlib, selectors, shlex, signal, subprocess, sys, time

class PTY:
    def __init__(self, binary, root, tag):
        self.root = root / tag
        self.root.mkdir(mode=0o700)
        env = dict(os.environ, HOME=str(self.root), SHELL='/bin/sh', PS1='PLYRA_TEST$ ', TERM='dumb')
        for key in ('ENV', 'BASH_ENV', 'ZDOTDIR', 'PROMPT_COMMAND'):
            env.pop(key, None)
        self.p = subprocess.Popen([str(binary), '--cols', '80', '--rows', '24'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, bufsize=0)
        os.set_blocking(self.p.stdout.fileno(), False)
        self.selector = selectors.DefaultSelector(); self.selector.register(self.p.stdout, selectors.EVENT_READ)
        self.buf=b''; self.events=[]; self.rows={}; self.sb=[]; self.ready=None; self.max_line=0; self.checks=[]
        self.until(lambda: self.ready is not None, 3)
        self.send("stty -echo; printf '\\033[2J\\033[H'; printf 'BACKEND_READY\\n'\n")
        self.until(lambda: 'BACKEND_READY' in self.text(), 3)
    def command(self, text):
        self.p.stdin.write(text.encode()+b'\n'); self.p.stdin.flush()
    def send(self, data):
        if isinstance(data,str): data=data.encode()
        self.command('input '+base64.b64encode(data).decode())
    def poll(self, timeout=.05):
        for key,_ in self.selector.select(timeout):
            chunk=os.read(key.fd,262144)
            if not chunk:
                self.selector.unregister(self.p.stdout)
                return
            self.buf+=chunk
            assert len(self.buf)<=2*1024*1024, 'protocol buffer unbounded'
            while b'\n' in self.buf:
                line,self.buf=self.buf.split(b'\n',1);self.max_line=max(self.max_line,len(line));assert len(line)<1024*1024
                e=json.loads(line);self.events.append(e)
                if e['type']=='ready': self.ready=e
                elif e['type']=='screen':
                    if e.get('full'): self.rows={}
                    for row in e['lines']:
                        assert 0<=row['row']<e['rows']
                        assert sum(x['width'] for x in row['runs']) == e['cols']
                        self.rows[row['row']]=row['runs']
                elif e['type']=='scrollback': self.sb.extend(e['lines']);self.sb=self.sb[-2000:]
                elif e['type']=='scrollback_clear': self.sb=[]
    def until(self, predicate, seconds=4):
        deadline=time.monotonic()+seconds
        while not predicate() and time.monotonic()<deadline:
            self.poll()
            if self.p.poll() is not None and not self.selector.select(0): break
        assert predicate(), {'timeout':seconds,'tail':self.text()[-1600:],'events':self.events[-2:]}
    def text(self):
        lines=[''.join(r['text'] for r in x['runs']) for x in self.sb]
        lines+=[''.join(r['text'] for r in self.rows[k]) for k in sorted(self.rows)]
        return '\n'.join(lines)
    def check(self, name, condition):
        assert condition, name
        self.checks.append(name)
    def finish(self, eof=False):
        if self.p.poll() is None:
            if eof:self.p.stdin.close()
            else:self.command('close')
        self.until(lambda:self.p.poll() is not None,3)
        while self.selector.select(.05):self.poll(0)
        stderr=self.p.stderr.read()
        self.check('helper exit zero',self.p.returncode==0)
        self.check('helper stderr empty',stderr==b'')
        self.selector.close()
        return {'tag':self.root.name,'checks':self.checks,'events':len(self.events),'max_jsonl_bytes':self.max_line,'shell_pid':self.ready['pid'],'stderr':stderr.decode(errors='replace')}

def pid_live(pid):
    p=subprocess.run(['/bin/ps','-p',str(pid),'-o','stat='],capture_output=True,text=True)
    return p.returncode==0 and p.stdout.strip() and not p.stdout.strip().startswith('Z')


def exit_backpressure_case(binary, root, reply_count=60000):
    """Keep stdout backpressured across child exit, then drain the final PTY bytes."""
    tag = f'exit-backpressure-{reply_count}'
    case_root = root / tag
    case_root.mkdir(mode=0o700)
    process = subprocess.Popen(
        [str(binary), '--exec', '/bin/sh', '-c',
         "sleep 0.2; printf 'EXIT_BACKPRESSURE_TAIL'; exit 9"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        env=dict(os.environ, HOME=str(case_root)), bufsize=0,
    )
    try:
        ready = json.loads(process.stdout.readline())
        assert ready['type'] == 'ready'
        # Exercise queues below and above the 2 MiB PTY-read high-water mark.
        process.stdin.write(b'unknown\n' * reply_count)
        process.stdin.flush()
        time.sleep(1.2)
        output = bytearray()
        os.set_blocking(process.stdout.fileno(), False)
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            deadline = time.monotonic() + 5
            eof = False
            while not eof and time.monotonic() < deadline:
                for key, _ in selector.select(0.1):
                    chunk = os.read(key.fd, 262144)
                    if chunk:
                        output.extend(chunk)
                        assert len(output) < 8 * 1024 * 1024
                    else:
                        eof = True
            assert eof, 'helper did not finish draining within five seconds'
        process.wait(timeout=1)
        events = [json.loads(line) for line in output.splitlines()]
        text = ''.join(run['text'] for event in events if event['type'] == 'screen'
                       for line in event['lines'] for run in line['runs'])
        assert 'EXIT_BACKPRESSURE_TAIL' in text, 'final PTY output was lost under backpressure'
        assert any(event['type'] == 'exit' and event['code'] == 9 for event in events)
        assert process.returncode == 0 and process.stderr.read() == b''
        return {'tag': tag, 'shell_pid': ready['pid'],
                'events': len(events), 'checks': [
                    'final PTY output survives consumer backpressure across child exit',
                    'natural child exit code survives deferred reap',
                    'helper exits cleanly after draining']}
    finally:
        if process.poll() is None:
            process.stdin.close()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()

def main():
    ap=argparse.ArgumentParser();ap.add_argument('--binary',required=True,type=pathlib.Path);ap.add_argument('--output',required=True,type=pathlib.Path);args=ap.parse_args()
    args.output.mkdir(parents=True,exist_ok=False);cases=[];active=[]
    try:
        t=PTY(args.binary,args.output,'interactive');active.append(t)
        t.check('ready protocol and shell',t.ready['protocol']==1 and t.ready['shell']=='/bin/sh')
        t.send("test -t 0 && test -t 1 && test -t 2 && printf 'ALL_ISATTY\\n'; printf 'TERM=%s HOME=%s\\n' \"$TERM\" \"$PWD\"\n")
        t.until(lambda:'ALL_ISATTY' in t.text() and 'TERM=xterm-256color' in t.text())
        t.check('all standard descriptors are a PTY','ALL_ISATTY' in t.text())
        t.check('cwd isolated HOME',str(t.root) in t.text().replace('\n',''))
        t.send("printf '\\033[31mRED_COLOR\\033[0m \\033[1;3;4mSTYLE_ATTR\\033[0m 中文 é\\n'; printf '\\033]0;Plyra 测试\\007'\n")
        t.until(lambda:'中文' in t.text() and any(e.get('title')=='Plyra 测试' for e in t.events))
        runs=[r for e in t.events if e['type']=='screen' for l in e['lines'] for r in l['runs']]
        t.check('ANSI red rendering',any('RED_COLOR' in r['text'] and r['fg']=='#bb0000' for r in runs))
        t.check('bold italic underline',any('STYLE_ATTR' in r['text'] and r['bold'] and r['italic'] and r['underline'] for r in runs))
        t.check('UTF8 wide and combining', '中文 é' in t.text())
        t.check('wide grapheme runs preserve exact width', any(r['text']=='中' and r['width']==2 for r in runs))
        t.check('combining grapheme run shares one cell', any(r['text']=='é' and r['width']==1 for r in runs))
        t.send("printf '\\033[0mBG_DEFAULT\\033[48;2;0;0;0mBG_EXPLICIT\\033[49mBG_RESET\\n'; printf '\\033[0;7mBG_REVERSE\\033[0m\\n'\n")
        t.until(lambda:'BG_REVERSE' in t.text())
        bg_runs=[r for e in t.events if e['type']=='screen' for l in e['lines'] for r in l['runs']]
        t.check('default background semantic flag', any('BG_DEFAULT' in r['text'] and r['bg']=='#000000' and r.get('bg_default') is True for r in bg_runs))
        t.check('explicit same RGB stays distinct opaque', any('BG_EXPLICIT' in r['text'] and r['bg']=='#000000' and r.get('bg_default') is False for r in bg_runs))
        t.check('SGR49 restores default flag', any('BG_RESET' in r['text'] and r.get('bg_default') is True for r in bg_runs))
        t.check('reverse preserves explicit inverse semantics', any('BG_REVERSE' in r['text'] and r.get('bg_default') is True and r['inverse'] for r in bg_runs))

        t.command('resize 96 20');t.send("stty size; printf 'RESIZE_DONE\\n'\n")
        t.until(lambda:'20 96' in t.text() and any(e.get('cols')==96 and e.get('rows')==20 for e in t.events))
        t.check('resize reaches PTY and screen',True)
        t.send("trap 'printf WINCH_OBSERVED\\\\n' WINCH\n");time.sleep(.05);t.command('resize 95 19')
        t.until(lambda:'WINCH_OBSERVED' in t.text());t.check('SIGWINCH delivered to shell',True)
        t.send("sleep 30\n");time.sleep(.12);t.send(b'\x03');t.send("printf 'CTRL_C_RECOVERED\\n'\n")
        t.until(lambda:'CTRL_C_RECOVERED' in t.text());t.check('CtrlC interrupts foreground job',True)
        t.send("printf '\\033[?1049hALTSCREEN_MARK\\n'\n")
        t.until(lambda:any(e.get('alternate') is True for e in t.events));t.check('alternate screen enters',True)
        t.send("printf '\\033[?1049lNORMALSCREEN_MARK\\n'\n")
        t.until(lambda:'NORMALSCREEN_MARK' in t.text() and t.events[-1].get('alternate') is False);t.check('alternate screen leaves',True)
        raw=t.root/'raw.py'
        raw.write_text("import os,termios,tty\nf=0\ns=termios.tcgetattr(f)\ntry:\n tty.setraw(f)\n os.write(1,b'\\x1b[?1h\\x1b[?2004hRAW_READY')\n data=b''\n while len(data)<20: data+=os.read(f,20-len(data))\nfinally:\n termios.tcsetattr(f,termios.TCSANOW,s)\n os.write(1,b'\\x1b[?1l\\x1b[?2004l\\r\\nRAW_HEX='+data.hex().encode()+b'\\r\\n')\n")
        t.send(shlex.quote(sys.executable)+' '+shlex.quote(str(raw))+'\n');t.until(lambda:'RAW_READY' in t.text())
        t.command('key Up 0');t.command('paste '+base64.b64encode(b'a\nbcd').decode())
        expect=b'\x1bOA\x1b[200~a\nbcd\x1b[201~'
        assert len(expect)==20
        t.until(lambda:'RAW_HEX='+expect.hex() in t.text().replace('\n',''))
        t.check('application cursor key and bracketed paste exactbytes',True)
        before=sum(e['type']=='error' for e in t.events)
        t.command('resize 1 2');t.command('input !!!!');t.command('unknown');t.command('input AA=A');t.command('key ')
        t.until(lambda:sum(e['type']=='error' for e in t.events)>=before+5);t.check('invalid protocol rejected without crash',True)
        t.send("i=0; while [ $i -lt 350 ]; do printf 'SCROLL_%04d\\n' \"$i\"; i=$((i+1)); done; printf 'SCROLL_DONE\\n'\n")
        t.until(lambda:'SCROLL_DONE' in t.text(),8);t.check('scrollback and bounded output',bool(t.sb) and all(e.get('dropped',0)>=0 for e in t.events))
        t.send("printf '\\033[3JCLEAR_HISTORY_DONE\\n'\n");t.until(lambda:any(e['type']=='scrollback_clear' for e in t.events));t.check('erase saved lines event',True)
        t.send('exit 37\n');t.until(lambda:any(e['type']=='exit' and e['code']==37 for e in t.events));t.check('shell exit code propagated',True)
        cases.append(t.finish());active.remove(t)
        for eof in (False,True):
            t=PTY(args.binary,args.output,'cleanup-eof' if eof else 'cleanup-close');active.append(t)
            t.send("sleep 60 & printf 'BACKGROUND_PID=%s\\n' $!\n")
            t.until(lambda:'BACKGROUND_PID=' in t.text())
            import re
            m=re.search(r'BACKGROUND_PID=(\d+)',t.text());assert m
            child=int(m.group(1));t.check('background fixture running',bool(pid_live(child)))
            receipt=t.finish(eof=eof)
            deadline=time.monotonic()+2
            while (pid_live(child) or pid_live(t.ready['pid'])) and time.monotonic()<deadline:time.sleep(.05)
            assert not pid_live(child) and not pid_live(t.ready['pid']), 'owned PTY session leaked'
            receipt['checks'].append('foreground/background session processes gone');receipt['background_pid']=child
            cases.append(receipt);active.remove(t)
        for reply_count in (4000, 60000):
            cases.append(exit_backpressure_case(args.binary, args.output, reply_count))
        result={'schema':'plyra.terminal.backend-tests','version':1,'passed':True,'binary':str(args.binary),'binary_sha256':hashlib.sha256(args.binary.read_bytes()).hexdigest(),'platform':sys.platform,'cases':cases,'checks':sum(len(x['checks']) for x in cases),'physical_device_acceptance':False,'linux_runtime_acceptance':False}
        (args.output/'RESULT.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result,indent=2))
    finally:
        for t in active:
            try:t.p.stdin.close();t.p.wait(timeout=3)
            except Exception:t.p.kill();t.p.wait()
if __name__=='__main__':main()

# FVPROV1 13654 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYmU5OGRiYzViMzE1NTU1MjczZmVmY2EzN2ZhZWQ4NWUzMmZiMzAzMmFlNDZiYTg1NzBmZjlhYTAyMjAzNzhlZiIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZGEyNjEyMjZjNWM4YWVmNDBmODM4ZTdiM2MxNWMyNjk2YzU4MDM3MzRmMjU1MTc5YmQ2NjJiZDdkNDE1MjIzMGM2ZTk0Y2VkNTRkMjAyYjI4Y2Q2YjE2YTk1ZjRmNjliODUwYWViNDU1Y2FjODk4YmQ2MzAxZTAwNjk3YjcyMDQifV19XQ== FVPROV1-END

#!/usr/bin/env python3
"""Bounded actual-window probe. Coordinate GUI ownership before invoking."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from test_profile import create_profile, preserve_and_clean

ROOT=Path(__file__).resolve().parents[1]

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--godot',default='/Applications/Godot.app/Contents/MacOS/Godot')
    parser.add_argument('--output',required=True,type=Path)
    args=parser.parse_args()
    output=args.output.absolute()
    if output.exists(): raise SystemExit('Choose a new output directory')
    output.mkdir(parents=True)
    stage=output/'project';stage.mkdir()
    for folder in ['scripts','shaders','assets','standalone','tests']:
        shutil.copytree(ROOT/folder,stage/folder,ignore=shutil.ignore_patterns('*.import','*.uid','.DS_Store'))
    profile_name,profile=create_profile(output,'Native')
    config=(ROOT/'project.godot').read_text().replace('[application]\n','[application]\nconfig/use_custom_user_dir=true\nconfig/custom_user_dir_name='+json.dumps(profile_name)+'\n',1)
    (stage/'project.godot').write_text(config)
    fingerprint={str(p.relative_to(stage)):hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(stage.rglob('*')) if p.is_file()}
    (output/'STAGED-SOURCE.json').write_text(json.dumps(fingerprint,indent=2)+'\n')
    env=dict(os.environ,SHELL='/bin/sh',ENV='',HISTFILE='/dev/null',PS1='PLYRA_TEST$ ')
    command=[args.godot,'--path',str(stage),'--rendering-method','mobile','--rendering-driver','metal','--script','res://tests/native_window_probe.gd','--','--probe-output='+str(output)]
    result=subprocess.run(command,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,timeout=30)
    (output/'godot.log').write_text(result.stdout)
    report_path=output/'WINDOW-RESULT.json'
    window=json.loads(report_path.read_text()) if report_path.exists() else {}
    pids=[int(p) for p in window.get('owned_pids',{}).values() if int(p)>0]
    absent=False
    if len(pids)==2:
        observed=subprocess.run(['/bin/ps','-p',','.join(map(str,pids)),'-o','pid='],capture_output=True,text=True)
        absent=observed.returncode in (0,1) and not observed.stdout.strip()
    passed=result.returncode==0 and window.get('passed') is True and absent and window.get('profile')==str(profile) and window.get('rendering_driver')=='metal' and (output/'terminal-native.png').is_file() and 'SCRIPT ERROR:' not in result.stdout
    receipt={'schema':'plyra.terminal.native-window-check','version':1,'passed':passed,'exitcode':result.returncode,'owned_pids':pids,'owned_pids_absent':absent,'window':window,'home_unchanged':True,'physical_input':False,'phone':False,'source_git_commit':subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),'runtime_manifest_sha256':hashlib.sha256((ROOT/'runtime-manifest.json').read_bytes()).hexdigest()}
    if absent: receipt['profile_cleanup']=preserve_and_clean(profile,output)
    (output/'RESULT.json').write_text(json.dumps(receipt,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps({'passed':passed,'owned_pids_absent':absent,'output':str(output)},indent=2))
    return 0 if passed else 1
if __name__=='__main__': sys.exit(main())

# FVPROV1 3309 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZTk3ZWQ4NzViZTBjYTMwODZiZDgxZmVlNzUxOWU0ZTcyMzFiOGFhODJjYTI0NTExNzJlMjhkMDQyZjcxZjUzMSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNzY2ZGU5OGY5NGI2Y2ZkNjhjMDliNjVkNDU4ZTI1OTBiOGIxZGFhMjhhNGJmOTcwMmU0ZWZiYWM0ZTVkNjczOWMwZGVkNmJiOWM2ZjQ1M2YyNzU3NmZiN2M0ZTBkNWYzZTBkNDAzOGY3N2U2MTY0Yzg2NGU0ZTEyNzg4YTQyMDUifV19XQ== FVPROV1-END

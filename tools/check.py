#!/usr/bin/env python3
"""One headless verification pass in a fresh ordinary uniquely owned profile.

No HOME override, device connection, VM, package installation or GUI launch.
"""
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
    if output.exists(): raise SystemExit('Choose a new output directory; existing evidence is never overwritten')
    output.mkdir(parents=True)
    stage=output/'project';stage.mkdir()
    for folder in ['scripts','shaders','assets','standalone','tests']:
        shutil.copytree(ROOT/folder,stage/folder,ignore=shutil.ignore_patterns('*.import','*.uid','.DS_Store'))
    profile_name,profile=create_profile(output,'Check')
    config=(ROOT/'project.godot').read_text().replace('[application]\n','[application]\nconfig/use_custom_user_dir=true\nconfig/custom_user_dir_name='+json.dumps(profile_name)+'\n',1)
    (stage/'project.godot').write_text(config)
    manifest=json.loads((ROOT/'runtime-manifest.json').read_text())
    for entry in manifest['files']+manifest['source_files']:
        path=ROOT/entry['path']
        if path.is_symlink() or hashlib.sha256(path.read_bytes()).hexdigest()!=entry['sha256']: raise SystemExit('Manifest mismatch: '+str(path))
    helper=json.loads((ROOT/'assets/terminal/manifest.json').read_text())
    for rel,digest in helper['build_source_files'].items():
        if hashlib.sha256((ROOT/rel).read_bytes()).hexdigest()!=digest: raise SystemExit('Helper source mismatch: '+rel)
    env=dict(os.environ,SHELL='/bin/sh',ENV='',HISTFILE='/dev/null',PS1='PLYRA_TEST$ ')
    runs=[];all_passed=True
    for script,extra in [('profile_probe.gd',[]),('terminal_screen_test.gd',[]),('terminal_session_test.gd',['--terminal-live']),('terminal_view_test.gd',[]),('standalone_close_test.gd',[])]:
        command=[args.godot,'--headless','--path',str(stage),'--script','res://tests/'+script,'--',*extra]
        try:
            result=subprocess.run(command,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,timeout=24)
        except subprocess.TimeoutExpired as timeout:
            (output/(script+'.log')).write_bytes(timeout.stdout or b'')
            raise SystemExit('Bounded Godot test timed out: '+script)
        (output/(script+'.log')).write_text(result.stdout)
        rows=[]
        for line in result.stdout.splitlines():
            try: rows.append(json.loads(line))
            except json.JSONDecodeError: pass
        accepted=result.returncode==0 and bool(rows) and not any('SCRIPT ERROR:' in line or line.startswith('ERROR:') for line in result.stdout.splitlines())
        if script=='profile_probe.gd': accepted=accepted and any(row.get('profile')==str(profile) for row in rows)
        pids=[]
        for row in rows:
            if row.get('event')=='owned_close': pids.extend(row['pids'].values())
            if row.get('test')=='standalone_close': pids.extend([row.get('helper',-1),row.get('shell',-1)])
        if pids:
            observed=subprocess.run(['/bin/ps','-p',','.join(str(p) for p in pids if p>0),'-o','pid='],capture_output=True,text=True)
            absent=observed.returncode in (0,1) and not observed.stdout.strip()
            accepted=accepted and absent
        else: absent=None
        all_passed=all_passed and accepted
        runs.append({'script':script,'returncode':result.returncode,'passed':accepted,'owned_pids':pids,'owned_pids_absent':absent,'reports':rows})
    receipt={'schema':'plyra.terminal.standalone-check','version':1,'passed':all_passed,'runtime_manifest_sha256':hashlib.sha256((ROOT/'runtime-manifest.json').read_bytes()).hexdigest(),'source_sha256':{str(p.relative_to(ROOT)):hashlib.sha256(p.read_bytes()).hexdigest() for folder in ('standalone','tests') for p in sorted((ROOT/folder).rglob('*')) if p.is_file() and p.suffix in ('.gd','.tscn')},'profile':str(profile),'home_unchanged':True,'headless':True,'native_pty':True,'gpu':False,'physical_input':False,'linux_device':False,'runs':runs}
    if all_passed: receipt['profile_cleanup']=preserve_and_clean(profile,output)
    (output/'RESULT.json').write_text(json.dumps(receipt,ensure_ascii=False,indent=2)+'\n')
    print(json.dumps({'passed':all_passed,'output':str(output),'runs':[{k:r[k] for k in ('script','passed','returncode')} for r in runs]},indent=2))
    return 0 if all_passed else 1
if __name__=='__main__': sys.exit(main())

# FVPROV1 4783 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNDU0NGI0MmVlMWJmZDI1MTc5NWM0YjgwOGEyMTZhM2JiOTY5M2ZkYjM3NDI3ZjM5NTA0YWNiZjI5OTVlODE1MCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYjgyZWUzZWFlNzJjNDNkMDI0MjUyYjNlYTA1NTM5NjE0OTZmOTczNzdiMTgwOGZhMWE5YmNjN2M3NWZhZTg5M2I0ZDM4NjY2YTJhNGRiNTQyZjIwY2MxODU5ZjNkMzQ1NDJiZjM2MzI1NzFhYjFkMTUxZGRiMmViYjBmNGI4MDMifV19XQ== FVPROV1-END

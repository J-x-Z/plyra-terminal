#!/usr/bin/env python3
"""Build the local PTY helper without installing software or contacting devices."""
import argparse, datetime, hashlib, json, os, pathlib, platform, shutil, struct, subprocess
ROOT=pathlib.Path(__file__).resolve().parents[3]
SOURCE=ROOT/'tools/mobile/terminal'
ASSETS=ROOT/'assets/terminal'
BUILD_ROOT=ROOT.parent/'plyra-terminal-builds'/'legacy-pty'
ARCHIVE_SHA='09156f43dd2128bd347cbeebe50d9a571d32c64e0cf18d211197946aff7226e0'

def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()
def copy_runtime_licenses():
    result={}
    for component,name in [('musl','LICENSE.musl'),('compiler_rt','LICENSE.compiler-rt')]:
        original=SOURCE/'vendor/runtime-licenses'/name
        destination=ASSETS/name
        if original.is_symlink() or not original.is_file():raise SystemExit('missing runtime license: '+name)
        shutil.copyfile(original,destination)
        if destination.read_bytes()!=original.read_bytes():raise SystemExit('runtime license copy mismatch: '+name)
        result[component]={'path':name,'sha256':sha(destination),'bytes':destination.stat().st_size}
    return result

def main():
    ap=argparse.ArgumentParser();ap.add_argument('--build-dir',type=pathlib.Path,default=BUILD_ROOT/'build-r1');args=ap.parse_args()
    out=args.build_dir.resolve()
    if not out.is_relative_to(BUILD_ROOT.resolve()):raise SystemExit('build-dir must be inside external terminal build root')
    if platform.system()!='Darwin' or platform.machine()!='arm64':raise SystemExit('macOS arm64 native build host required by this two-target recipe')
    zig=shutil.which('zig')
    if zig is None:raise SystemExit('Zig must be available on PATH')
    out.mkdir(parents=True,exist_ok=False)
    paths=[SOURCE/'plyra_terminal.c',*sorted((SOURCE/'vendor/libvterm/src').glob('*.c'))]
    before={str(p.relative_to(ROOT)):sha(p) for p in SOURCE.rglob('*') if p.is_file() and '__pycache__' not in p.parts}
    base=['-std=c99','-O2','-Wall','-Wextra','-Werror','-Wno-unused-parameter','-Wno-missing-field-initializers','-Wno-sign-compare','-I',str(SOURCE/'vendor/libvterm/include'),'-I',str(SOURCE/'vendor/libvterm/src')]
    commands={
        'macos_arm64':['/usr/bin/cc',*base,*map(str,paths),'-o',str(out/'plyra-terminal-macos-arm64')],
        'linux_arm64':[zig,'cc','-target','aarch64-linux-musl','-static','-s',*base,*map(str,paths),'-o',str(out/'plyra-terminal-linux-arm64')],
    }
    env=dict(os.environ,ZIG_LOCAL_CACHE_DIR=str(BUILD_ROOT/'zig-cache'),ZIG_GLOBAL_CACHE_DIR=str(BUILD_ROOT/'zig-global-cache'))
    receipts={}
    for target,cmd in commands.items():
        p=subprocess.run(cmd,stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=env,timeout=240)
        (out/(target+'.stdout')).write_bytes(p.stdout);(out/(target+'.stderr')).write_bytes(p.stderr)
        if p.returncode:raise SystemExit(f'{target} failed ({p.returncode}): '+p.stderr.decode(errors='replace'))
        artifact=pathlib.Path(cmd[-1]);data=artifact.read_bytes()
        if target=='macos_arm64':
            assert data[:4]==b'\xcf\xfa\xed\xfe' and struct.unpack_from('<I',data,4)[0]==0x0100000c
        else:
            assert data[:6]==b'\x7fELF\x02\x01' and struct.unpack_from('<H',data,18)[0]==183
            phoff=struct.unpack_from('<Q',data,32)[0];entsize=struct.unpack_from('<H',data,54)[0];count=struct.unpack_from('<H',data,56)[0]
            assert all(struct.unpack_from('<I',data,phoff+i*entsize)[0]!=3 for i in range(count)), 'ELF must have no interpreter'
        destination=ASSETS/'bin'/artifact.name;destination.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(artifact,destination);destination.chmod(0o755)
        receipts[target]={'path':'bin/'+artifact.name,'sha256':sha(destination),'bytes':len(data),'format':'Mach-O' if target=='macos_arm64' else 'ELF-static-musl'}
    license=SOURCE/'vendor/libvterm/LICENSE';shutil.copyfile(license,ASSETS/'LICENSE.libvterm')
    runtime_licenses=copy_runtime_licenses()
    manifest={'schema':'plyra.mobile.terminal-helper','version':1,'protocol':1,'runtime_files':receipts,'license_sha256':sha(ASSETS/'LICENSE.libvterm'),'runtime_licenses':runtime_licenses,'library':{'name':'libvterm','version':'0.3.3','archive_sha256':ARCHIVE_SHA,'source_url':'https://www.leonerd.org.uk/code/libvterm/libvterm-0.3.3.tar.gz','license':'MIT'},'build_source_files':before,'limits':{'cols':[10,240],'rows':[4,100],'input_chunk_bytes':65536,'jsonl_event_bytes':1048576,'pending_output_bytes':8388608,'pending_input_bytes':262144,'pending_scrollback_rows':128},'platform_acceptance':{'macos_arm64':'build only; see backend-test RESULT for runtime','linux_arm64':'cross-built ELF only; no physical device acceptance'}}
    (ASSETS/'manifest.json').write_text(json.dumps(manifest,indent=2,ensure_ascii=False)+'\n')
    for rel,digest in before.items():assert sha(ROOT/rel)==digest,'source modified during build: '+rel
    receipt={'schema':'plyra.terminal.build','version':1,'passed':True,'recorded_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'source_files':before,'commands':commands,'runtime_files':receipts,'manifest_sha256':sha(ASSETS/'manifest.json'),'sources_unchanged':True,'linux_arm64_static_header_check':True,'linux_runtime_executed':False}
    (out/'RESULT.json').write_text(json.dumps(receipt,indent=2)+'\n');print(json.dumps(receipt,indent=2))
if __name__=='__main__':main()

# FVPROV1 5379 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYWM4YTBjZDBiOTFiNTE3YjU0YzFiN2FiNjczNDg1ZWVlMTA3ZTRlNDQ3MmRlMDIwMzE0M2MyNWIzNTQ5MDA0ZCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZTJlMjY1ODljODYyZTFlNWM3YWNmZjBkZWU0YzU0MzM3YzhlZDZjNTMxY2ViNDNlOWU3ODc3ZDNlNjkxNGI3MTkwNjg5MjlkNTZlYjQ1OWNiZTg5NmY3M2NlZDg0MzI3NjM0YjgzN2RlMTdkNzliZmIyZWZlZWYyYTA4Yzk3MGYifV19XQ== FVPROV1-END

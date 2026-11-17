"""Ordinary uniquely owned Godot test profile, never HOME or a symlink."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import uuid

MARKER='.plyra-terminal-owned-test-profile'

def create_profile(output,kind):
    name='PlyraTerminal'+kind+'-'+uuid.uuid4().hex
    base=Path.home()/'Library/Application Support' if sys.platform=='darwin' else Path(os.environ.get('XDG_DATA_HOME',str(Path.home()/'.local/share')))
    profile=base/name
    profile.mkdir(parents=True,exist_ok=False)
    (profile/MARKER).write_text(name+'\n')
    (output/'PROFILE.json').write_text(json.dumps({'name':name,'actual_path':str(profile),'isolated':True},indent=2)+'\n')
    return name,profile

def preserve_and_clean(profile,output):
    if profile.is_symlink() or (profile/MARKER).read_text()!=profile.name+'\n': raise RuntimeError('Profile ownership mismatch')
    files=sorted(path for path in profile.rglob('*') if path.is_file())
    if any(path.is_symlink() for path in profile.rglob('*')): raise RuntimeError('Unexpected profile symlink')
    hashes={str(path.relative_to(profile)):hashlib.sha256(path.read_bytes()).hexdigest() for path in files}
    destination=output/'profile-readback'
    shutil.copytree(profile,destination)
    for relative,digest in hashes.items():
        if hashlib.sha256((destination/relative).read_bytes()).hexdigest()!=digest: raise RuntimeError('Profile readback mismatch')
    shutil.rmtree(profile)
    return {'path':str(profile),'preserved':str(destination),'sha256':hashes,'removed':not profile.exists()}

# FVPROV1 1570 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYmQ3OTAxMmQ0YWY0Y2JmYmVjYmYyZmYyOGVkZDgxMTQ3YTJkMDYxMzkxMzAzMGU0ODM5NDQwNjYzNjQ5MjJmYyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYmZhMzQ2YTdhMGZiN2Y4ZWVkMTlkZjQxMDNiYmJhMDIzNjJlNjgwYmIyOWZkMTdlNzA1MzVkYzk0ZjEzZTFhZjNlODZiZjkwMDQyYjIzMzhkYTQzMjgxMTFkNWQ0MmZhMmRkNjkwZjRmZjI5ZWMyNWE3NzE2YTc4NTA3MjQ4MDkifV19XQ== FVPROV1-END

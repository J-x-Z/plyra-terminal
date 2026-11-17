#!/usr/bin/env python3
"""Refresh the explicit portable payload contract after an intentional upgrade."""
import hashlib
import json
from pathlib import Path
ROOT = Path(__file__).resolve().parents[1]

def record(path):
    return {"path": str(path.relative_to(ROOT)), "sha256": hashlib.sha256(path.read_bytes()).hexdigest(), "bytes": path.stat().st_size}

def payload_files():
    paths = list((ROOT / "scripts/terminal").glob("*.gd")) + [ROOT / "shaders/terminal/acrylic.gdshader"]
    for folder in ("assets/terminal", "assets/terminal-fonts"):
        paths.extend(
            path for path in (ROOT / folder).rglob("*")
            if path.is_file()
            and path.suffix not in (".import", ".uid")
            and path.name not in (".DS_Store", "GLASS_THIRD_PARTY_NOTICES.txt")
            and "upstream-notices" not in path.relative_to(ROOT / folder).parts
        )
    return sorted(paths)

def content():
    sources = sorted(path for path in (ROOT / "tools/mobile/terminal").rglob("*") if path.is_file() and "__pycache__" not in path.parts and path.suffix != ".pyc")
    return {"schema": "plyra.terminal.runtime", "version": 1, "api_version": 1,
            "origin": {"project": "plyra-mobile", "commit": "55ef8efc0914845124daf539db5daec64c2582b2"},
            "files": [record(path) for path in payload_files()],
            "source_files": [record(path) for path in sources]}
if __name__ == "__main__":
    (ROOT / "runtime-manifest.json").write_text(json.dumps(content(), indent=2) + "\n")

# FVPROV1 1514 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMTkxYjMyNzcyMzQwZTJlZGVjOGJiMmM3NmFiNzNiYzNmYTA2ZmZkNDVkNGM2N2Y3MmI1NWQyMGU4ZWEyMzM3MCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNjc1OTRiYWQ0ZWExYzYzMDVkYzMxYjkyYzM0MTVhNzcwY2VhZmQ4MmMwOWFiMTFiNTZiMTJkYTRjOGM0YWQ0OGZlNTY3ZGIxNTBhYmEyN2Q2YWMzYjk0NGU5OWJjYTM5NmU1MDc0NWEyYjNlMDhlYzViMTk0MjFkMWM4ZjVhMGQifV19XQ== FVPROV1-END

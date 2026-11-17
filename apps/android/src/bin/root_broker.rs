// SPDX-License-Identifier: GPL-3.0-only
#[cfg(target_os = "android")]
fn main() {
    if let Err(error) = privilege_broker::run() {
        eprintln!("Android broker failed: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "android"))]
fn main() {
    eprintln!("The Android broker requires an Android target.");
    std::process::exit(1);
}

// FVPROV1 369 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZTYzOGRiMTkzZjI0MzU4YTZhM2Y4NzkzY2Y4MmM5YTFkOTZjYTUxZTExMThiMDc0NTQ2ODQ2OTg1NDJjZWZlYyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZDVmNzg3ZmQzMDc4MDhkMjk1OWU5ZjY2OWVjNTAxZWEyZDI2OTIxZjgzZWRjNTU0MTI3MTcyZWIyZjlhMDRkYjhjMDFlNWZjMDdlMTJmZGI4ZGQ5NjhkM2IyNTg4MzYxMmVkMWRhMzg4NWM1NGM1YjI3ZWI0Y2VmOTBlMDQ4MDkifV19XQ== FVPROV1-END

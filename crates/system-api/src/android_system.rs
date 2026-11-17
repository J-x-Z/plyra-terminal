// SPDX-License-Identifier: GPL-3.0-only
//! Bounded logd reads. Android's logd independently enforces the caller's UID access.
use crate::{LogEntry, LogQuery};

#[cfg(target_os = "android")]
pub(crate) fn logs(query: LogQuery) -> anyhow::Result<Vec<LogEntry>> {
    use anyhow::ensure;
    ensure!(
        query
            .process
            .as_ref()
            .is_none_or(|value| value.len() <= 256 && !value.chars().any(char::is_control))
            && query
                .contains
                .as_ref()
                .is_none_or(|value| value.len() <= 1024 && !value.contains('\0')),
        "Android log filter exceeds its size limit or contains controls"
    );
    let uid = crate::current_uid();
    let output = crate::bounded_command(
        "/system/bin/logcat",
        &[
            "-d".into(),
            "-b".into(),
            "main".into(),
            "-v".into(),
            "threadtime,epoch,printable".into(),
            format!("--uid={uid}"),
            "-t".into(),
            "500".into(),
            "*:V".into(),
        ],
        std::time::Duration::from_secs(3),
        4 * 1024 * 1024,
    )?;
    ensure!(
        output.success,
        "logcat query failed for effective UID {uid}: {}",
        output.stderr
    );
    Ok(parse_logs(
        &output.stdout,
        &query,
        crate::now_ms(),
        uid,
        std::process::id(),
    ))
}

fn epoch_millis(value: &str) -> Option<u64> {
    let (seconds, fraction) = value.split_once('.')?;
    if fraction.is_empty() || !fraction.bytes().all(|value| value.is_ascii_digit()) {
        return None;
    }
    let milliseconds = format!("{fraction:0<3}");
    seconds
        .parse::<u64>()
        .ok()?
        .checked_mul(1000)?
        .checked_add(milliseconds[..3].parse().ok()?)
}

fn parse_logs(output: &str, query: &LogQuery, now: u64, uid: u32, own_pid: u32) -> Vec<LogEntry> {
    let cutoff = now.saturating_sub(u64::from(query.seconds) * 1000);
    let process = query.process.as_deref().unwrap_or_default().to_lowercase();
    let contains = query.contains.as_deref().unwrap_or_default().to_lowercase();
    let own_process = process == "glass-terminal" || process == "org.plyra.glass.terminal";
    let mut records = std::collections::VecDeque::new();
    for line in output.lines() {
        // threadtime+epoch emits epoch, PID, TID, priority, tag, then the message.
        let mut rest = line.trim_start();
        let mut field = || {
            let end = rest.find(char::is_whitespace)?;
            let value = &rest[..end];
            rest = rest[end..].trim_start();
            Some(value)
        };
        let Some(timestamp) = field() else { continue };
        let Some(sampled_at) = epoch_millis(timestamp) else {
            continue;
        };
        let Some(pid) = field().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        if field()
            .and_then(|value| value.parse::<u32>().ok())
            .is_none()
            || field().is_none()
        {
            continue;
        }
        let Some((tag, message)) = rest.split_once(':') else {
            continue;
        };
        let tag = tag.trim();
        let message = message.trim_start();
        if sampled_at < cutoff
            || sampled_at > now.saturating_add(1000)
            || (own_process && pid != own_pid)
            || (!process.is_empty()
                && !own_process
                && process != pid.to_string()
                && !tag.to_lowercase().contains(&process))
            || !message.to_lowercase().contains(&contains)
        {
            continue;
        }
        records.push_back(LogEntry {
            timestamp: format!("{timestamp} UTC epoch"),
            process: format!("{tag} · PID {pid} · logcat main / effective UID {uid} visible scope"),
            message: message.chars().take(8192).collect(),
        });
        if records.len() > query.limit {
            records.pop_front();
        }
    }
    records.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_logcat_filters_time_pid_message_and_entry_limit() {
        let output = "--------- beginning of main\n1000.001 7 8 I RustStdoutStderr: old\n1050.001 7 8 I RustStdoutStderr: 中文 first\n1051.999 99 8 I Other: 中文 other process\n1052.003 7 8 I RustStdoutStderr: 中文 last\nmalformed\n";
        let mut query = LogQuery {
            contains: Some("中文".into()),
            seconds: 30,
            limit: 1,
            ..Default::default()
        };
        let records = parse_logs(output, &query, 1_060_000, 10234, 7);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message, "中文 last");
        assert!(records[0].process.contains("effective UID 10234"));
        query.process = Some("other".into());
        assert_eq!(
            parse_logs(output, &query, 1_060_000, 0, 7)[0].message,
            "中文 other process"
        );
        assert!(parse_logs("1000.x 7 8 I tag: bad", &query, 1_060_000, 0, 7).is_empty());
    }
}

// FVPROV1 5110 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNGYxZGY4YWRhOGU2MjAwZjAwZGQwYTU5Yjc0YTViYTFlYmJhZjA1OTAxYmVjZTgzY2EzZDdmODVlMjc1NWVlNCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZTliNDVhNjYwODhkNTA2M2FmZTNmMDJkZDUwMTIxMGNiZDI4ZTU2YjAyYWE5MTMxNmY2YWYwNGE5Mzk2OTQ3MmVhMDk4ZmVhOGY2YzE3OTE0YzlmMGIzNzViMmY3YjhkZmI0MzZhZTFlZGIxYjE4ZTM5MjA4MTM0YTE3OGRjMDEifV19XQ== FVPROV1-END

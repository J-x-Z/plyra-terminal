// SPDX-License-Identifier: GPL-3.0-only
//! Bounded Linux observations and operations against the caller's user manager.
use crate::{
    bounded_command, current_uid, linux_admin, LogEntry, LogQuery, ServiceIdentity, ServiceInfo,
};
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

const SYSTEMCTL: &str = "/usr/bin/systemctl";
const PROPERTIES: &str = "Id,LoadState,MainPID,ExecMainStatus,NeedDaemonReload,FragmentPath,SourcePath,DropInPaths,Type,ExecStart,ExecStartPre,ExecStartPost,ExecStop,ExecStopPost,User,Group,WorkingDirectory,RootDirectory,Environment,EnvironmentFiles,Restart,KillMode,RemainAfterExit,TimeoutStopUSec";
const CONFIG_PROPERTIES: &[&str] = &[
    "Id",
    "LoadState",
    "NeedDaemonReload",
    "FragmentPath",
    "SourcePath",
    "DropInPaths",
    "Type",
    "ExecStart",
    "ExecStartPre",
    "ExecStartPost",
    "ExecStop",
    "ExecStopPost",
    "User",
    "Group",
    "WorkingDirectory",
    "RootDirectory",
    "Environment",
    "EnvironmentFiles",
    "Restart",
    "KillMode",
    "RemainAfterExit",
    "TimeoutStopUSec",
];

fn authorize_user(effective_uid: u32, uid: u32) -> Result<()> {
    ensure!(
        uid != 0 && (effective_uid == 0 || effective_uid == uid),
        "operation is outside the authenticated user manager"
    );
    Ok(())
}

fn account_name(uid: u32) -> Result<String> {
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    let mut buffer = vec![0_u8; 65_536];
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut found,
        )
    };
    ensure!(
        status == 0 && !found.is_null() && !entry.pw_name.is_null(),
        "caller account is unavailable"
    );
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) }
        .to_str()?
        .to_owned();
    ensure!(
        !name.is_empty()
            && !name.contains('@')
            && !name
                .chars()
                .any(|value| value.is_control() || value.is_whitespace()),
        "caller account cannot be represented as a systemd user-bus target"
    );
    Ok(name)
}

fn manager_arguments(effective_uid: u32, uid: u32, account: &str) -> Result<Vec<String>> {
    authorize_user(effective_uid, uid)?;
    let mut arguments = vec![
        "--user".into(),
        "--no-pager".into(),
        "--no-ask-password".into(),
    ];
    if effective_uid == 0 {
        arguments.push(format!("--machine={account}@.host"));
    }
    Ok(arguments)
}

fn valid_unit(label: &str) -> bool {
    !label.starts_with('-')
        && label.ends_with(".service")
        && label.len() <= 255
        && label
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || b"._-@:\\".contains(&value))
}

fn property_blocks(text: &str) -> Vec<BTreeMap<String, String>> {
    text.split("\n\n")
        .filter_map(|block| {
            let fields: BTreeMap<_, _> = block
                .lines()
                .filter_map(|line| line.split_once('='))
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect();
            fields.contains_key("Id").then_some(fields)
        })
        .collect()
}

fn configuration_hash(fields: &BTreeMap<String, String>) -> String {
    let mut hash = Sha256::new();
    for key in CONFIG_PROPERTIES {
        hash.update(key.as_bytes());
        hash.update([0]);
        hash.update(fields.get(*key).map_or("", String::as_str).as_bytes());
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())
}

fn show_units(uid: u32, label: &str) -> Result<Vec<BTreeMap<String, String>>> {
    authorize_user(current_uid(), uid)?;
    let account = account_name(uid)?;
    let mut arguments = manager_arguments(current_uid(), uid, &account)?;
    arguments.extend([
        "show".into(),
        "--all".into(),
        format!("--property={PROPERTIES}"),
        "--".into(),
        label.into(),
    ]);
    let output = bounded_command(
        SYSTEMCTL,
        &arguments,
        Duration::from_secs(3),
        4 * 1024 * 1024,
    )?;
    ensure!(
        output.success,
        "systemd user-manager query failed: {}",
        output.stderr
    );
    let blocks = property_blocks(&output.stdout);
    ensure!(
        blocks.len() <= 4000,
        "systemd user-manager response exceeds 4000 services"
    );
    Ok(blocks)
}

fn service_info(fields: &BTreeMap<String, String>, uid: u32) -> Result<ServiceInfo> {
    let label = fields.get("Id").context("unit identity is unavailable")?;
    ensure!(valid_unit(label), "invalid systemd service identity");
    ensure!(
        fields.get("LoadState").map(String::as_str) == Some("loaded"),
        "service is not loaded in the user manager"
    );
    let pid = fields
        .get("MainPID")
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid > 1);
    let process_start_time = pid.and_then(|pid| linux_admin::process_start_time(pid).ok());
    Ok(ServiceInfo {
        identity: ServiceIdentity {
            label: label.clone(),
            domain: format!("user/{uid}"),
            uid,
            pid,
            process_start_time,
            owner_sid: None,
            configuration_hash: Some(configuration_hash(fields)),
        },
        pid,
        last_exit: fields
            .get("ExecMainStatus")
            .and_then(|value| value.parse().ok()),
    })
}

pub fn user_services(uid: u32) -> Result<Vec<ServiceInfo>> {
    show_units(uid, "*.service")?
        .iter()
        .filter(|fields| fields.get("LoadState").map(String::as_str) == Some("loaded"))
        .map(|fields| service_info(fields, uid))
        .collect()
}

fn same_service(expected: &ServiceIdentity, current: &ServiceIdentity) -> bool {
    expected.uid == current.uid
        && expected.domain == current.domain
        && expected.label == current.label
        && expected.pid == current.pid
        && expected.process_start_time == current.process_start_time
        && expected.owner_sid == current.owner_sid
        && expected.configuration_hash.is_some()
        && expected.configuration_hash == current.configuration_hash
}

pub fn validate_service(expected: &ServiceIdentity, caller_uid: u32) -> Result<()> {
    authorize_user(current_uid(), caller_uid)?;
    ensure!(
        expected.uid == caller_uid
            && expected.domain == format!("user/{caller_uid}")
            && valid_unit(&expected.label),
        "service is outside the authenticated user manager"
    );
    ensure!(
        expected.pid.is_none() || expected.process_start_time.is_some(),
        "service process identity is unavailable; refresh before restarting"
    );
    let blocks = show_units(caller_uid, &expected.label)?;
    ensure!(
        blocks.len() == 1,
        "service no longer has a unique manager identity"
    );
    let fields = &blocks[0];
    ensure!(
        fields.get("NeedDaemonReload").map(String::as_str) != Some("yes"),
        "service files changed; reload the user manager and refresh before restarting"
    );
    let current = service_info(fields, caller_uid)?.identity;
    ensure!(
        same_service(expected, &current),
        "service configuration or process identity changed"
    );
    Ok(())
}

pub fn restart_user_service(expected: &ServiceIdentity, caller_uid: u32) -> Result<()> {
    validate_service(expected, caller_uid)?;
    let account = account_name(caller_uid)?;
    let mut arguments = manager_arguments(current_uid(), caller_uid, &account)?;
    arguments.extend(["restart".into(), "--".into(), expected.label.clone()]);
    let output = bounded_command(SYSTEMCTL, &arguments, Duration::from_secs(5), 65_536)?;
    ensure!(
        output.success,
        "systemd user-service restart failed: {}",
        output.stderr
    );
    Ok(())
}

pub fn logs(uid: u32, query: LogQuery) -> Result<Vec<LogEntry>> {
    ensure!(
        uid == current_uid() && uid != 0,
        "user journal queries require the ordinary user's context"
    );
    ensure!(
        (1..=300).contains(&query.seconds) && (1..=500).contains(&query.limit),
        "journal query exceeds the 300-second/500-entry limit"
    );
    ensure!(
        query.process.is_some() || query.contains.is_some(),
        "journal query requires a process or message filter"
    );
    let mut arguments = vec![
        "--user".into(),
        "--no-pager".into(),
        "--quiet".into(),
        "--output=json".into(),
        "--lines=500".into(),
        format!("--since=-{}s", query.seconds),
    ];
    if let Some(process) = &query.process {
        ensure!(
            !process.is_empty() && process.len() <= 256 && !process.chars().any(char::is_control),
            "invalid journal process filter"
        );
        arguments.push(format!(
            "{}={process}",
            if process.starts_with('/') {
                "_EXE"
            } else {
                "_COMM"
            }
        ));
    }
    if let Some(contains) = &query.contains {
        ensure!(
            !contains.is_empty()
                && contains.len() <= 256
                && !contains.chars().any(char::is_control),
            "invalid journal message filter"
        );
    }
    let output = bounded_command(
        "/usr/bin/journalctl",
        &arguments,
        Duration::from_secs(5),
        4 * 1024 * 1024,
    )?;
    ensure!(
        output.success,
        "user journal query failed: {}",
        output.stderr
    );
    parse_journal(&output.stdout, query.contains.as_deref(), query.limit)
}

fn parse_journal(text: &str, contains: Option<&str>, limit: usize) -> Result<Vec<LogEntry>> {
    let needle = contains.map(str::to_lowercase);
    let mut records = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value =
            serde_json::from_str(line).context("journal response is not JSON-lines")?;
        let message = value["MESSAGE"]
            .as_str()
            .unwrap_or("[binary journal message]");
        if needle
            .as_ref()
            .is_some_and(|needle| !message.to_lowercase().contains(needle))
        {
            continue;
        }
        let micros = value["__REALTIME_TIMESTAMP"]
            .as_str()
            .and_then(|value| value.parse::<u64>().ok())
            .context("journal timestamp is unavailable")?;
        records.push(LogEntry {
            timestamp: format!("unix {}.{:06} UTC", micros / 1_000_000, micros % 1_000_000),
            process: value["_COMM"]
                .as_str()
                .or_else(|| value["SYSLOG_IDENTIFIER"].as_str())
                .unwrap_or_default()
                .into(),
            message: message.chars().take(8192).collect(),
        });
    }
    if records.len() > limit {
        drop(records.drain(..records.len() - limit));
    }
    Ok(records)
}

pub(crate) fn endpoint(text: &str, ipv6: bool) -> Result<String> {
    let (address, port) = text
        .split_once(':')
        .context("invalid proc socket endpoint")?;
    ensure!(
        address.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid proc socket address"
    );
    let port = u16::from_str_radix(port, 16)?;
    let address = if ipv6 {
        ensure!(address.len() == 32, "invalid IPv6 socket address");
        let mut bytes = [0_u8; 16];
        for (index, chunk) in bytes.chunks_exact_mut(4).enumerate() {
            chunk.copy_from_slice(
                &u32::from_str_radix(&address[index * 8..index * 8 + 8], 16)?.to_ne_bytes(),
            );
        }
        IpAddr::V6(Ipv6Addr::from(bytes))
    } else {
        ensure!(address.len() == 8, "invalid IPv4 socket address");
        IpAddr::V4(Ipv4Addr::from(
            u32::from_str_radix(address, 16)?.to_ne_bytes(),
        ))
    };
    Ok(std::net::SocketAddr::new(address, port).to_string())
}

pub(crate) fn socket_state(state: &str, protocol: &str) -> &'static str {
    match state {
        "01" => "ESTABLISHED",
        "02" => "SYN_SENT",
        "03" => "SYN_RECV",
        "04" => "FIN_WAIT1",
        "05" => "FIN_WAIT2",
        "06" => "TIME_WAIT",
        "07" if protocol.starts_with("udp") => "UNCONN",
        "07" => "CLOSE",
        "08" => "CLOSE_WAIT",
        "09" => "LAST_ACK",
        "0A" => "LISTEN",
        "0B" => "CLOSING",
        "0C" => "NEW_SYN_RECV",
        _ => "UNKNOWN",
    }
}

fn proc_connections(text: &str, protocol: &str, uid: u32) -> Result<Vec<String>> {
    let mut rows = Vec::new();
    for line in text.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().collect();
        ensure!(fields.len() >= 10, "invalid proc socket table row");
        if fields[7].parse::<u32>()? != uid {
            continue;
        }
        let state = socket_state(fields[3], protocol);
        let ipv6 = protocol.ends_with('6');
        rows.push(format!(
            "{protocol}\t{}\t{}\t{state}\tuid={uid}\tinode={}",
            endpoint(fields[1], ipv6)?,
            endpoint(fields[2], ipv6)?,
            fields[9]
        ));
    }
    Ok(rows)
}

pub fn connections(uid: u32) -> Result<String> {
    authorize_user(current_uid(), uid)?;
    let mut rows = Vec::new();
    let mut observed = false;
    for protocol in ["tcp", "tcp6", "udp", "udp6"] {
        let path = format!("/proc/net/{protocol}");
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("cannot read {path}")),
        };
        observed = true;
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "proc socket table exceeds its 1 MiB budget"
        );
        rows.extend(proc_connections(
            std::str::from_utf8(&bytes)?,
            protocol,
            uid,
        )?);
    }
    ensure!(
        observed,
        "proc socket observations are unavailable in this network namespace"
    );
    ensure!(
        rows.len() <= 1000,
        "current-user socket observation exceeds 1000 rows"
    );
    rows.sort();
    Ok(rows.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_selects_the_callers_user_bus_and_foreign_users_are_denied() {
        assert_eq!(
            manager_arguments(501, 501, "owner").unwrap(),
            vec!["--user", "--no-pager", "--no-ask-password"]
        );
        assert_eq!(
            manager_arguments(0, 501, "owner").unwrap().last().unwrap(),
            "--machine=owner@.host"
        );
        assert!(manager_arguments(501, 502, "other").is_err());
        assert!(manager_arguments(0, 0, "root").is_err());
    }

    #[test]
    fn configuration_fingerprint_and_target_identity_detect_changes() {
        let mut fields = BTreeMap::from([
            ("Id".into(), "fixture.service".into()),
            ("ExecStart".into(), "/usr/bin/sleep 60".into()),
        ]);
        let before = configuration_hash(&fields);
        fields.insert("MainPID".into(), "42".into());
        assert_eq!(configuration_hash(&fields), before);
        fields.insert("ExecStart".into(), "/usr/bin/sleep 120".into());
        assert_ne!(configuration_hash(&fields), before);
        let expected = ServiceIdentity {
            label: "fixture.service".into(),
            domain: "user/501".into(),
            uid: 501,
            pid: Some(42),
            process_start_time: Some(100),
            owner_sid: None,
            configuration_hash: Some(before),
        };
        let mut current = expected.clone();
        assert!(same_service(&expected, &current));
        current.process_start_time = Some(101);
        assert!(!same_service(&expected, &current));
        current = expected.clone();
        current.configuration_hash = Some(configuration_hash(&fields));
        assert!(!same_service(&expected, &current));
        assert!(!valid_unit("--system.service"));
        assert!(!valid_unit("bad\nname.service"));
    }

    #[test]
    fn journal_json_is_filtered_and_bounded_without_regex_interpretation() {
        let text = "{\"__REALTIME_TIMESTAMP\":\"1000001\",\"_COMM\":\"fixture\",\"MESSAGE\":\"first 中文\"}\n{\"__REALTIME_TIMESTAMP\":\"2000002\",\"_COMM\":\"fixture\",\"MESSAGE\":\"second 中文\"}\n";
        let entries = parse_journal(text, Some("中文"), 1).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, "second 中文");
        assert_eq!(entries[0].timestamp, "unix 2.000002 UTC");
        assert!(parse_journal(text, Some(".*"), 10).unwrap().is_empty());
    }

    #[test]
    fn proc_socket_rows_keep_only_the_requested_user_and_decode_endpoints() {
        let table = "header\n0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000 501 0 12345\n1: 0100007F:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000 502 0 12346\n";
        let rows = proc_connections(table, "tcp", 501).unwrap();
        assert_eq!(
            rows,
            vec!["tcp\t127.0.0.1:8080\t0.0.0.0:0\tLISTEN\tuid=501\tinode=12345"]
        );
        assert_eq!(
            endpoint("00000000000000000000000001000000:0016", true).unwrap(),
            "[::1]:22"
        );
    }
}

// FVPROV1 17622 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNmViMGU2NzYwNmE1NGVlOGNjYzA1YTI4OTIwYWIxYzI3NmZkMWUxOWEwODBjYTZiMDE2MmZlMDlhMjM3ZDFjOCIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMmZjMWRiODE3YWIwZmU5ZjE5MThlNzJjMzg2ZjUwYTgzNzEzOTBhODFiYjdjZDg0ZTFkZDljMTQ0NzU0ZTU4ZmI2NjMzN2M4YjQ5ODkyNTVjMWY2NGI0OWEwZmMzZjAyZWM0MzYzMWY2NmE0ZTAyMWNmMmViNWZkOWYwYzY3MDYifV19XQ== FVPROV1-END

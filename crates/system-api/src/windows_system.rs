// SPDX-License-Identifier: GPL-3.0-only
use super::windows_identity;
use crate::{LogEntry, LogQuery};
use anyhow::{ensure, Result};
use std::{
    ffi::OsStr,
    net::{Ipv4Addr, Ipv6Addr},
    ptr,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        GetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, ERROR_TIMEOUT, FILETIME,
        SYSTEMTIME,
    },
    NetworkManagement::IpHelper::*,
    Networking::WinSock::{AF_INET, AF_INET6},
    System::{EventLog::*, Time::FileTimeToSystemTime},
};

struct Event(EVT_HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            EvtClose(self.0);
        }
    }
}

unsafe fn event_text(pointer: *const u16) -> Result<String> {
    ensure!(!pointer.is_null(), "event provider name is missing");
    let mut length = 0;
    while length < 65_536 && *pointer.add(length) != 0 {
        length += 1;
    }
    ensure!(length < 65_536, "event string exceeds its limit");
    Ok(String::from_utf16(std::slice::from_raw_parts(
        pointer, length,
    ))?)
}

fn event_message(event: EVT_HANDLE, provider: &str, event_id: u16) -> Result<String> {
    let provider = windows_identity::wide(OsStr::new(provider))?;
    unsafe {
        let metadata = EvtOpenPublisherMetadata(0, provider.as_ptr(), ptr::null(), 0, 0);
        if metadata == 0 {
            return Ok(format!(
                "Event ID {event_id} (provider message resources unavailable)"
            ));
        }
        let metadata = Event(metadata);
        let mut needed = 0;
        EvtFormatMessage(
            metadata.0,
            event,
            0,
            0,
            ptr::null(),
            EvtFormatMessageEvent,
            0,
            ptr::null_mut(),
            &mut needed,
        );
        if needed == 0 || needed > 65_536 {
            return Ok(format!(
                "Event ID {event_id} (message unavailable or oversized)"
            ));
        }
        let mut message = vec![0_u16; needed as usize];
        if EvtFormatMessage(
            metadata.0,
            event,
            0,
            0,
            ptr::null(),
            EvtFormatMessageEvent,
            needed,
            message.as_mut_ptr(),
            &mut needed,
        ) == 0
        {
            return Ok(format!(
                "Event ID {event_id} (message formatting unavailable)"
            ));
        }
        let length = message
            .iter()
            .position(|ch| *ch == 0)
            .unwrap_or(message.len());
        Ok(String::from_utf16_lossy(&message[..length])
            .chars()
            .take(4096)
            .collect())
    }
}

fn render_event(context: EVT_HANDLE, event: EVT_HANDLE) -> Result<LogEntry> {
    unsafe {
        let mut needed = 0;
        let mut properties = 0;
        EvtRender(
            context,
            event,
            EvtRenderEventValues,
            0,
            ptr::null_mut(),
            &mut needed,
            &mut properties,
        );
        ensure!(
            needed > 0 && needed <= 256 * 1024,
            "event system values exceed their budget"
        );
        let mut bytes = vec![0_u64; (needed as usize).div_ceil(8)];
        ensure!(
            EvtRender(
                context,
                event,
                EvtRenderEventValues,
                needed,
                bytes.as_mut_ptr().cast(),
                &mut needed,
                &mut properties
            ) != 0,
            "cannot render Windows event ({})",
            std::io::Error::last_os_error()
        );
        ensure!(
            (properties as usize) <= bytes.len() * 8 / std::mem::size_of::<EVT_VARIANT>()
                && properties > EvtSystemTimeCreated as u32,
            "invalid Windows event values"
        );
        let values =
            std::slice::from_raw_parts(bytes.as_ptr().cast::<EVT_VARIANT>(), properties as usize);
        ensure!(
            values[EvtSystemProviderName as usize].Type == EvtVarTypeString as u32
                && values[EvtSystemTimeCreated as usize].Type == EvtVarTypeFileTime as u32,
            "Windows event has unexpected system-value types"
        );
        let provider = event_text(values[EvtSystemProviderName as usize].Anonymous.StringVal)?;
        let id = values[EvtSystemEventID as usize].Anonymous.UInt16Val;
        let raw = values[EvtSystemTimeCreated as usize].Anonymous.FileTimeVal;
        let time = FILETIME {
            dwLowDateTime: raw as u32,
            dwHighDateTime: (raw >> 32) as u32,
        };
        let mut date: SYSTEMTIME = std::mem::zeroed();
        ensure!(
            FileTimeToSystemTime(&time, &mut date) != 0,
            "invalid Windows event timestamp"
        );
        Ok(LogEntry {
            timestamp: format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
                date.wYear,
                date.wMonth,
                date.wDay,
                date.wHour,
                date.wMinute,
                date.wSecond,
                date.wMilliseconds
            ),
            message: event_message(event, &provider, id)?,
            process: provider,
        })
    }
}

/// Reads local Application/System event logs. The process filter matches provider names.
pub fn logs(query: LogQuery) -> Result<Vec<LogEntry>> {
    ensure!(
        (1..=3600).contains(&query.seconds) && (1..=500).contains(&query.limit),
        "invalid event-log query budget"
    );
    let selection = windows_identity::wide(OsStr::new(&format!(
        "*[System[TimeCreated[timediff(@SystemTime) <= {}]]]",
        u64::from(query.seconds) * 1000
    )))?;
    let process = query.process.as_ref().map(|text| text.to_lowercase());
    let contains = query.contains.as_ref().map(|text| text.to_lowercase());
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut result = Vec::new();
    let mut inspected = 0;
    let mut text_bytes = 0;
    unsafe {
        let context = EvtCreateRenderContext(0, ptr::null(), EvtRenderContextSystem);
        ensure!(context != 0, "cannot create Windows event render context");
        let context = Event(context);
        for channel in ["Application", "System"] {
            let channel = windows_identity::wide(OsStr::new(channel))?;
            let events = EvtQuery(
                0,
                channel.as_ptr(),
                selection.as_ptr(),
                EvtQueryChannelPath | EvtQueryReverseDirection,
            );
            ensure!(
                events != 0,
                "cannot query Windows event channel ({})",
                std::io::Error::last_os_error()
            );
            let events = Event(events);
            let mut channel_entries = 0;
            while channel_entries < query.limit
                && inspected < 10_000
                && text_bytes < 256 * 1024
                && Instant::now() < deadline
            {
                let mut batch = [0_isize; 16];
                let mut count = 0;
                if EvtNext(
                    events.0,
                    batch.len() as u32,
                    batch.as_mut_ptr(),
                    50,
                    0,
                    &mut count,
                ) == 0
                {
                    match GetLastError() {
                        ERROR_NO_MORE_ITEMS => break,
                        ERROR_TIMEOUT => continue,
                        _ => anyhow::bail!(
                            "Windows event iteration failed ({})",
                            std::io::Error::last_os_error()
                        ),
                    }
                }
                ensure!(count as usize <= batch.len(), "invalid Windows event batch");
                let batch: Vec<_> = batch[..count as usize].iter().copied().map(Event).collect();
                for event in batch {
                    inspected += 1;
                    if channel_entries >= query.limit
                        || text_bytes >= 256 * 1024
                        || Instant::now() >= deadline
                    {
                        break;
                    }
                    let entry = render_event(context.0, event.0)?;
                    if process
                        .as_ref()
                        .is_some_and(|filter| !entry.process.to_lowercase().contains(filter))
                        || contains
                            .as_ref()
                            .is_some_and(|filter| !entry.message.to_lowercase().contains(filter))
                    {
                        continue;
                    }
                    text_bytes += entry.message.len() + entry.process.len();
                    result.push(entry);
                    channel_entries += 1;
                }
            }
        }
    }
    result.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
    result.truncate(query.limit);
    Ok(result)
}

fn table(mut query: impl FnMut(*mut std::ffi::c_void, &mut u32) -> u32) -> Result<Vec<u64>> {
    let mut needed = 0;
    let first = query(ptr::null_mut(), &mut needed);
    ensure!(
        (first == 0 || first == ERROR_INSUFFICIENT_BUFFER)
            && (4..=2 * 1024 * 1024).contains(&needed),
        "Windows endpoint table size is unavailable or oversized"
    );
    for _ in 0..4 {
        let mut bytes = vec![0_u64; (needed as usize).div_ceil(8)];
        let code = query(bytes.as_mut_ptr().cast(), &mut needed);
        if code == 0 {
            return Ok(bytes);
        }
        ensure!(
            code == ERROR_INSUFFICIENT_BUFFER && needed <= 2 * 1024 * 1024,
            "Windows endpoint table query failed ({code})"
        );
    }
    anyhow::bail!("Windows endpoint table changed repeatedly during sampling")
}

unsafe fn rows<T>(bytes: &[u64], offset: usize) -> Result<&[T]> {
    let count = *bytes.as_ptr().cast::<u32>() as usize;
    ensure!(
        offset <= bytes.len() * 8 && count <= (bytes.len() * 8 - offset) / std::mem::size_of::<T>(),
        "Windows endpoint table contains an invalid count"
    );
    Ok(std::slice::from_raw_parts(
        bytes.as_ptr().cast::<u8>().add(offset).cast::<T>(),
        count,
    ))
}

fn port(raw: u32) -> u16 {
    u16::from_be(raw as u16)
}

fn ipv6(address: [u8; 16], scope: u32, raw_port: u32) -> String {
    let address = Ipv6Addr::from(address);
    if scope == 0 {
        format!("[{address}]:{}", port(raw_port))
    } else {
        format!("[{address}%{scope}]:{}", port(raw_port))
    }
}

pub fn connections() -> Result<String> {
    let mut output = vec!["Windows GetExtendedTcpTable/GetExtendedUdpTable snapshot; numeric endpoints and owning PID; maximum 2000 rows".into()];
    unsafe {
        let tcp4 = table(|buffer, size| {
            GetExtendedTcpTable(buffer, size, 0, AF_INET as u32, TCP_TABLE_OWNER_PID_ALL, 0)
        })?;
        for row in rows::<MIB_TCPROW_OWNER_PID>(
            &tcp4,
            std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table),
        )?
        .iter()
        .take(2000)
        {
            output.push(format!(
                "TCP4 PID {} {}:{} -> {}:{} state {}",
                row.dwOwningPid,
                Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()),
                port(row.dwLocalPort),
                Ipv4Addr::from(row.dwRemoteAddr.to_ne_bytes()),
                port(row.dwRemotePort),
                row.dwState
            ));
        }
        let tcp6 = table(|buffer, size| {
            GetExtendedTcpTable(buffer, size, 0, AF_INET6 as u32, TCP_TABLE_OWNER_PID_ALL, 0)
        })?;
        for row in rows::<MIB_TCP6ROW_OWNER_PID>(
            &tcp6,
            std::mem::offset_of!(MIB_TCP6TABLE_OWNER_PID, table),
        )?
        .iter()
        .take(2001 - output.len())
        {
            output.push(format!(
                "TCP6 PID {} {} -> {} state {}",
                row.dwOwningPid,
                ipv6(row.ucLocalAddr, row.dwLocalScopeId, row.dwLocalPort),
                ipv6(row.ucRemoteAddr, row.dwRemoteScopeId, row.dwRemotePort),
                row.dwState
            ));
        }
        let udp4 = table(|buffer, size| {
            GetExtendedUdpTable(buffer, size, 0, AF_INET as u32, UDP_TABLE_OWNER_PID, 0)
        })?;
        for row in rows::<MIB_UDPROW_OWNER_PID>(
            &udp4,
            std::mem::offset_of!(MIB_UDPTABLE_OWNER_PID, table),
        )?
        .iter()
        .take(2001 - output.len())
        {
            output.push(format!(
                "UDP4 PID {} {}:{}",
                row.dwOwningPid,
                Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()),
                port(row.dwLocalPort)
            ));
        }
        let udp6 = table(|buffer, size| {
            GetExtendedUdpTable(buffer, size, 0, AF_INET6 as u32, UDP_TABLE_OWNER_PID, 0)
        })?;
        for row in rows::<MIB_UDP6ROW_OWNER_PID>(
            &udp6,
            std::mem::offset_of!(MIB_UDP6TABLE_OWNER_PID, table),
        )?
        .iter()
        .take(2001 - output.len())
        {
            output.push(format!(
                "UDP6 PID {} {}",
                row.dwOwningPid,
                ipv6(row.ucLocalAddr, row.dwLocalScopeId, row.dwLocalPort)
            ));
        }
    }
    Ok(output.join("\n"))
}

// FVPROV1 13343 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiN2I3OTE3NTIyZTUxM2FmZWE2OTdhMjdhYTQxN2JlYjIwZjdmZDZlYTRlOTkzMGZiZWQ4NTUxZGQ4MTg1ZmM2NSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNTgwNzQ3NmE2Zjk3Nzc0Mzg0YjhjNDEzOTRlMzZmNjMwNWQ3ZmQwY2FkYTY3NjZmZTU1YzY1ZGI4Mjg5ZTRkM2RiNTVlMTM4ZDM0MjQ1YzNjNDEyNTgzMzU3ZWUyZjVjZTA0ODg0N2E1ODhjZmI2NzFhZjQ5NWJmY2ZjN2NiMDYifV19XQ== FVPROV1-END

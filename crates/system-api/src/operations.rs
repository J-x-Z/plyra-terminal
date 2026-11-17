// SPDX-License-Identifier: GPL-3.0-only
//! Shared read operation catalog for the GUI and structured Shell adapter.
use crate::{BrokerClient, LogEntry, LogQuery, PrivilegedAction, SystemClient, SystemSnapshot};
use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Serialize)]
pub struct OperationDefinition {
    pub schema: &'static str,
    pub kind: &'static str,
    pub name: &'static str,
    pub summary: &'static str,
    pub usage: &'static str,
    pub effect: &'static str,
    pub input_type: &'static str,
    pub help: &'static str,
    pub parameters: &'static [ParameterDefinition],
    pub output_type: &'static str,
}

#[derive(Clone, Copy, Serialize)]
pub struct ParameterDefinition {
    pub name: &'static str,
    pub value_type: &'static str,
    pub required: bool,
    pub help: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum: Option<u64>,
}

const LOG_PARAMETERS: &[ParameterDefinition] = &[
    ParameterDefinition {
        name: "--process",
        value_type: "string",
        required: false,
        help: "Filter native process name",
        minimum: None,
        maximum: None,
    },
    ParameterDefinition {
        name: "--contains",
        value_type: "string",
        required: false,
        help: "Filter message text",
        minimum: None,
        maximum: None,
    },
    ParameterDefinition {
        name: "--seconds",
        value_type: "integer",
        required: false,
        help: "Look back this many seconds",
        minimum: Some(1),
        maximum: Some(300),
    },
    ParameterDefinition {
        name: "--limit",
        value_type: "integer",
        required: false,
        help: "Maximum returned records",
        minimum: Some(1),
        maximum: Some(500),
    },
];

pub const CATALOG: &[OperationDefinition] = &[
    definition(
        "sys.catalog",
        "List system read operations",
        "sys.catalog",
        "operation.definition",
        &[],
    ),
    definition(
        "sys.processes",
        "Read native process identities and measurements",
        "sys.processes",
        "system.object/process",
        &[],
    ),
    definition(
        "sys.services",
        "Read current-user services",
        "sys.services",
        "system.object/service",
        &[],
    ),
    definition(
        "sys.network",
        "Read cumulative native interface counters",
        "sys.network",
        "system.object/network",
        &[],
    ),
    definition(
        "sys.devices",
        "Read reported storage and USB devices",
        "sys.devices",
        "system.object/device",
        &[],
    ),
    definition("sys.operations", "Discover operations for system objects", "sys.operations [process|service]", "operation.definition", &[]),
    definition("sys.metrics", "Read memory, thermal and reported GPU counters", "sys.metrics", "system.object/metrics", &[]),
    definition("sys.subscribe", "Subscribe to native process, file or network events", "sys.subscribe process|files|network [--pid PID] [--directory PATH] [--seconds N] [--limit N] [--interval-ms N]", "system.event", &[]),
    definition("sys.complete", "Read service and device completion candidates", "sys.complete service|device [PREFIX]", "shell.completion", &[]),
    broker_definition("sys.prepare", "Prepare a process.stop or service.restart operation", "sys.prepare ACTION OBJECT_JSON", "system.object", "broker.prepared", "prepare"),
    broker_definition("sys.confirm", "Confirm one prepared operation ticket", "sys.confirm TICKET", "broker.ticket", "broker.result", "execute"),
    broker_definition("sys.cancel", "Cancel pending operation tickets and retain the authorization session", "sys.cancel", "none", "broker.result", "cancel"),
    broker_definition("sys.revoke", "Revoke the current Broker authorization", "sys.revoke", "none", "broker.result", "revoke"),
    broker_definition("sys.audit", "Read the current Broker audit records", "sys.audit", "none", "broker.audit", "read"),
    broker_definition("sys.power", "Read one bounded whole-SoC estimated power sample", "sys.power [INTERVAL_MS]", "none", "system.power", "read"),
    broker_definition("sys.identity", "Read the authenticated Broker session identity", "sys.identity", "none", "broker.identity", "read"),
    definition(
        "sys.logs",
        "Read bounded native system logs",
        "sys.logs [--process NAME] [--contains TEXT] [--seconds 1..300] [--limit 1..500]",
        "system.object/log",
        LOG_PARAMETERS,
    ),
];
const fn definition(
    name: &'static str,
    summary: &'static str,
    usage: &'static str,
    output_type: &'static str,
    parameters: &'static [ParameterDefinition],
) -> OperationDefinition {
    OperationDefinition {
        schema: "glass.operation.v1",
        kind: "operation.definition",
        name,
        summary,
        usage,
        effect: "read",
        input_type: "none",
        help: summary,
        parameters,
        output_type,
    }
}

const fn broker_definition(
    name: &'static str,
    summary: &'static str,
    usage: &'static str,
    input_type: &'static str,
    output_type: &'static str,
    effect: &'static str,
) -> OperationDefinition {
    OperationDefinition {
        input_type,
        effect,
        ..definition(name, summary, usage, output_type, &[])
    }
}

pub const OBJECT_OPERATIONS: &[OperationDefinition] = &[
    broker_definition(
        "process.stop",
        "Send SIGTERM to the identified process",
        "sys.prepare process.stop OBJECT_JSON",
        "system.object/process",
        "broker.prepared",
        "signal",
    ),
    broker_definition(
        "service.restart",
        "Restart the identified current-user service",
        "sys.prepare service.restart OBJECT_JSON",
        "system.object/service",
        "broker.prepared",
        "restart",
    ),
];

pub fn action_from_object(name: &str, value: &Value) -> Result<PrivilegedAction> {
    let identity = value.get("identity").unwrap_or(value).clone();
    let expected_type = match name {
        "process.stop" => "process",
        "service.restart" => "service",
        _ => bail!("unknown system object operation {name}"),
    };
    if let Some(kind) = value.get("object_type") {
        ensure!(
            kind.as_str() == Some(expected_type),
            "operation object type does not match its target"
        );
    }
    let action = match name {
        "process.stop" => PrivilegedAction::StopProcess(
            serde_json::from_value(identity).context("invalid native process identity")?,
        ),
        "service.restart" => PrivilegedAction::RestartUserService(
            serde_json::from_value(identity).context("invalid native service identity")?,
        ),
        _ => unreachable!(),
    };
    if let Some(claimed) = value.get("object_id") {
        let identity = match &action {
            PrivilegedAction::StopProcess(identity) => serde_json::to_value(identity)?,
            PrivilegedAction::RestartUserService(identity) => serde_json::to_value(identity)?,
        };
        ensure!(
            claimed.as_str() == Some(object_id(expected_type, &identity).as_str()),
            "system object identity does not match its object reference"
        );
    }
    Ok(action)
}

/// The GUI gateway and ordinary stdio adapter use the same authenticated client.
/// No operation in this function installs a helper or requests system authentication.
pub fn broker_records(broker: &BrokerClient, name: &str, args: &[String]) -> Result<Value> {
    match name {
        "sys.prepare" => {
            ensure!(
                args.len() == 2,
                "sys.prepare requires ACTION and OBJECT_JSON"
            );
            let object: Value =
                serde_json::from_str(&args[1]).context("invalid operation object JSON")?;
            let action = action_from_object(&args[0], &object)?;
            Ok(serde_json::to_value(broker.prepare(action)?)?)
        }
        "sys.confirm" => {
            ensure!(args.len() == 1, "sys.confirm requires one prepared ticket");
            Ok(serde_json::to_value(broker.confirm(&args[0])?)?)
        }
        "sys.cancel" => {
            ensure!(args.is_empty(), "sys.cancel accepts no arguments");
            broker.cancel_pending()?;
            Ok(json!({"success":true,"message":"Pending operation tickets cancelled"}))
        }
        "sys.revoke" => {
            ensure!(args.is_empty(), "sys.revoke accepts no arguments");
            broker.revoke()?;
            Ok(json!({"success":true,"message":"Broker authorization revoked"}))
        }
        "sys.audit" => {
            ensure!(args.is_empty(), "sys.audit accepts no arguments");
            Ok(serde_json::to_value(broker.audit()?)?)
        }
        "sys.power" => {
            ensure!(
                args.len() <= 1,
                "sys.power accepts one optional interval in milliseconds"
            );
            let interval = args
                .first()
                .map(|value| value.parse::<u16>().context("invalid power interval"))
                .transpose()?
                .unwrap_or(1000);
            Ok(serde_json::to_value(broker.power_sample(interval)?)?)
        }
        "sys.identity" => {
            ensure!(args.is_empty(), "sys.identity accepts no arguments");
            Ok(serde_json::to_value(broker.session_identity()?)?)
        }
        _ => bail!("unknown Broker operation {name}"),
    }
}

pub fn is_broker_operation(name: &str) -> bool {
    matches!(
        name,
        "sys.prepare"
            | "sys.confirm"
            | "sys.cancel"
            | "sys.revoke"
            | "sys.audit"
            | "sys.identity"
            | "sys.power"
    )
}

pub enum ReadQuery {
    Snapshot,
    Processes,
    Services,
    Network,
    Devices,
    Logs(LogQuery),
}
pub enum ReadResult {
    Snapshot(SystemSnapshot),
    Logs(Vec<LogEntry>),
    Objects(Vec<Value>),
}

pub fn execute(client: &SystemClient, query: ReadQuery) -> Result<ReadResult> {
    match query {
        ReadQuery::Snapshot => client.snapshot().map(ReadResult::Snapshot),
        ReadQuery::Logs(query) => client.logs(query).map(ReadResult::Logs),
        ReadQuery::Services => {
            let now = crate::now_ms();
            client
                .services()?
                .into_iter()
                .map(|service| {
                    let identity = serde_json::to_value(&service.identity)?;
                    let mut value = serde_json::to_value(&service)?;
                    if let Some(object) = value.as_object_mut() {
                        object.extend(identity.as_object().unwrap().clone());
                    }
                    Ok(object(
                        "service",
                        &identity,
                        value,
                        now,
                        "native current-user service manager",
                        None,
                        "native-service-identity",
                    ))
                })
                .collect::<Result<Vec<_>>>()
                .map(ReadResult::Objects)
        }
        query => {
            let snapshot = client.snapshot()?;
            let now = snapshot.sampled_at_ms;
            let source = snapshot.source.as_str();
            let values = match query {
                ReadQuery::Processes => {
                    let ids: std::collections::BTreeMap<_, _> = snapshot
                        .processes
                        .iter()
                        .map(|process| {
                            let identity = serde_json::to_value(&process.identity).unwrap();
                            (process.identity.pid, object_id("process", &identity))
                        })
                        .collect();
                    snapshot
                        .processes
                        .iter()
                        .map(|process| {
                            let identity = serde_json::to_value(&process.identity)?;
                            let mut value = serde_json::to_value(process)?;
                            value
                                .as_object_mut()
                                .unwrap()
                                .extend(identity.as_object().unwrap().clone());
                            let parent = process.parent_pid.and_then(|pid| ids.get(&pid).cloned());
                            Ok(object(
                                "process",
                                &identity,
                                value,
                                now,
                                source,
                                parent,
                                "native-pid-start-uid-exe",
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?
                }
                ReadQuery::Network => snapshot
                    .network
                    .iter()
                    .map(|network| {
                        let identity = json!({"name": network.name});
                        Ok(object(
                            "network",
                            &identity,
                            serde_json::to_value(network)?,
                            now,
                            source,
                            None,
                            "reported-interface-name",
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
                ReadQuery::Devices => snapshot
                    .devices
                    .iter()
                    .enumerate()
                    .map(|(index, device)| {
                        let identity = json!({
                            "sampled_at_ms": now,
                            "index": index,
                            "name": device.name,
                            "kind": device.kind,
                        });
                        let value = json!({
                            "name": device.name,
                            "device_kind": device.kind,
                            "details": device.details,
                        });
                        Ok(object(
                            "device",
                            &identity,
                            value,
                            now,
                            source,
                            None,
                            "snapshot-local",
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
                _ => unreachable!(),
            };
            Ok(ReadResult::Objects(values))
        }
    }
}

pub fn snapshot(client: &SystemClient) -> Result<SystemSnapshot> {
    match execute(client, ReadQuery::Snapshot)? {
        ReadResult::Snapshot(snapshot) => Ok(snapshot),
        _ => unreachable!(),
    }
}
pub fn logs(client: &SystemClient, query: LogQuery) -> Result<Vec<LogEntry>> {
    match execute(client, ReadQuery::Logs(query))? {
        ReadResult::Logs(logs) => Ok(logs),
        _ => unreachable!(),
    }
}

pub fn records(client: &SystemClient, name: &str, args: &[String]) -> Result<Vec<Value>> {
    if name == "sys.catalog" {
        ensure!(args.is_empty(), "sys.catalog accepts no arguments");
        return CATALOG
            .iter()
            .map(|definition| serde_json::to_value(definition).map_err(Into::into))
            .collect();
    }
    if name == "sys.operations" {
        return object_operation_records(args);
    }
    if name == "sys.complete" {
        return completion_records(client, args);
    }
    if name == "sys.metrics" {
        ensure!(args.is_empty(), "sys.metrics accepts no arguments");
        return Ok(vec![metrics_record()?]);
    }
    let filter = if ["sys.services", "sys.devices"].contains(&name) && !args.is_empty() {
        ensure!(
            args.len() == 2 && args[0] == "--name",
            "system object filter requires --name NAME"
        );
        Some(args[1].as_str())
    } else {
        None
    };
    let query = if name == "sys.logs" {
        ReadQuery::Logs(log_query(args)?)
    } else {
        ensure!(
            args.is_empty() || filter.is_some(),
            "this system read operation accepts no arguments"
        );
        match name {
            "sys.processes" => ReadQuery::Processes,
            "sys.services" => ReadQuery::Services,
            "sys.network" => ReadQuery::Network,
            "sys.devices" => ReadQuery::Devices,
            _ => bail!("unknown system read operation {name}"),
        }
    };
    match execute(client, query)? {
        ReadResult::Objects(values) => Ok(values
            .into_iter()
            .filter(|value| {
                filter.is_none_or(|selected| {
                    value[if name == "sys.services" {
                        "label"
                    } else {
                        "name"
                    }]
                    .as_str()
                        == Some(selected)
                })
            })
            .collect()),
        ReadResult::Logs(logs) => {
            let now = crate::now_ms();
            logs.into_iter()
                .enumerate()
                .map(|(index, entry)| {
                    let identity = json!({
                        "timestamp": entry.timestamp,
                        "process": entry.process,
                        "message": entry.message,
                        "index": index,
                    });
                    Ok(object(
                        "log",
                        &identity,
                        serde_json::to_value(entry)?,
                        now,
                        "native bounded log query",
                        None,
                        "reported-log-entry",
                    ))
                })
                .collect()
        }
        ReadResult::Snapshot(_) => unreachable!(),
    }
}

fn log_query(args: &[String]) -> Result<LogQuery> {
    let mut query = LogQuery::default();
    let mut args = args.iter();
    while let Some(option) = args.next() {
        let value = args.next().context("system log option requires a value")?;
        match option.as_str() {
            "--process" => query.process = Some(value.clone()),
            "--contains" => query.contains = Some(value.clone()),
            "--seconds" => query.seconds = value.parse().context("invalid log seconds")?,
            "--limit" => query.limit = value.parse().context("invalid log limit")?,
            _ => bail!("unknown log option {option}"),
        }
    }
    ensure!(
        (1..=300).contains(&query.seconds) && (1..=500).contains(&query.limit),
        "system log query outside supported bounds"
    );
    Ok(query)
}
fn object_id(kind: &str, identity: &Value) -> String {
    let hash = Sha256::digest(serde_json::to_vec(identity).unwrap());
    format!(
        "{kind}:{}",
        hash.iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}
fn object(
    kind: &str,
    identity: &Value,
    mut value: Value,
    sampled_at: u64,
    source: &str,
    parent: Option<String>,
    identity_scope: &str,
) -> Value {
    let fields = value.as_object_mut().unwrap();
    fields.insert("schema".into(), "glass.system.object.v1".into());
    fields.insert("kind".into(), "system.object".into());
    fields.insert("object_type".into(), kind.into());
    fields.insert("object_id".into(), object_id(kind, identity).into());
    fields.insert("sampled_at_ms".into(), sampled_at.into());
    fields.insert("source".into(), source.into());
    fields.insert(
        "parent_id".into(),
        parent.map_or(Value::Null, Value::String),
    );
    fields.insert("identity_scope".into(), identity_scope.into());
    let operations: Vec<_> = OBJECT_OPERATIONS.iter().filter(|operation| operation.input_type == format!("system.object/{kind}")).map(|operation| json!({
        "name":operation.name,"effect":operation.effect,"prepare":"sys.prepare","confirm":"sys.confirm","requires_confirmation":true,
        "authorization":"existing Broker current-user or explicitly authorized administrator session",
    })).collect();
    fields.insert("operations".into(), Value::Array(operations));
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operation_catalog_and_log_options_are_shared_and_bounded() {
        assert!(CATALOG.iter().any(|entry| entry.name == "sys.processes"));
        assert_eq!(
            log_query(&["--seconds".into(), "2".into(), "--limit".into(), "5".into()])
                .unwrap()
                .limit,
            5
        );
        assert!(log_query(&["--limit".into(), "0".into()]).is_err());
        assert!(log_query(&["--unknown".into(), "x".into()]).is_err());
    }
    #[test]
    fn process_object_identity_excludes_measurement_changes() {
        let identity = json!({"pid":9,"start_time":7,"uid":"501","exe":"/bin/sleep"});
        let a = object(
            "process",
            &identity,
            json!({"cpu_percent":0}),
            1,
            "test",
            None,
            "native",
        );
        let b = object(
            "process",
            &identity,
            json!({"cpu_percent":20}),
            2,
            "test",
            None,
            "native",
        );
        assert_eq!(a["object_id"], b["object_id"]);
        assert_eq!(a["kind"], "system.object");
    }

    #[test]
    fn partial_process_stop_object_reference_is_checked_before_preparation() {
        let identity = json!({"pid":42,"start_time":7,"uid":"0","exe":""});
        let record = object(
            "process",
            &identity,
            json!({"identity":identity.clone()}),
            1,
            "test",
            None,
            "native-pid-start-uid-exe",
        );
        let PrivilegedAction::StopProcess(expected) =
            action_from_object("process.stop", &record).unwrap()
        else {
            panic!("expected a process stop action");
        };
        assert!(expected.exe.is_empty());
        assert_eq!(expected.uid, "0");
        assert_eq!(expected.start_time, 7);
        for (field, changed) in [
            ("pid", json!(43)),
            ("start_time", json!(8)),
            ("uid", json!("501")),
            ("exe", json!("/forged/executable")),
        ] {
            let mut forged = record.clone();
            forged["identity"][field] = changed;
            let error = action_from_object("process.stop", &forged).err().unwrap();
            assert!(error.to_string().contains("object reference"));
        }
    }
}

pub fn object_operation_records(args: &[String]) -> Result<Vec<Value>> {
    ensure!(
        args.len() <= 1,
        "sys.operations accepts an optional object type"
    );
    return OBJECT_OPERATIONS
        .iter()
        .filter(|operation| {
            args.first()
                .is_none_or(|kind| operation.input_type == format!("system.object/{kind}"))
        })
        .map(|operation| serde_json::to_value(operation).map_err(Into::into))
        .collect();
}

pub fn completion_records(client: &SystemClient, args: &[String]) -> Result<Vec<Value>> {
    ensure!(
        (1..=2).contains(&args.len()),
        "sys.complete requires service|device and an optional prefix"
    );
    let prefix = args.get(1).map_or("", String::as_str);
    let object_value = args[0].ends_with("-object");
    let kind = match args[0].as_str() {
        "service" | "service-object" => "sys.services",
        "device" | "device-object" => "sys.devices",
        _ => bail!("unknown system completion provider"),
    };
    let values = records(client, kind, &[])?
        .into_iter()
        .filter_map(|object| {
            let value = object[if kind == "sys.services" { "label" } else { "name" }].as_str()?.to_owned();
            if !value.starts_with(prefix) { return None; }
            let description = if kind == "sys.services" {
                object["domain"].as_str().unwrap_or("native service")
            } else {
                object["device_kind"].as_str().unwrap_or("native device")
            };
            let insertion = if object_value { serde_json::to_string(&object).ok()? } else { value.clone() };
            if insertion.len() > 8192 { return None; }
            Some(json!({"schema":"glass.completion.v1","kind":if kind == "sys.services" {"service"} else {"device"},"provider":args[0],"value":insertion,"label":value,"description":description,"object":object}))
        })
        .take(128)
        .collect::<Vec<_>>();
    let mut total = 0;
    Ok(values
        .into_iter()
        .take_while(|value| {
            total += serde_json::to_vec(value).map_or(262145, |value| value.len() + 1);
            total <= 262144
        })
        .collect())
}

pub fn metrics_record() -> Result<Value> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let mut value = json!({
        "memory": {
            "total_bytes": system.total_memory(),
            "available_bytes": system.available_memory(),
            "used_bytes": system.used_memory(),
            "swap_total_bytes": system.total_swap(),
            "swap_used_bytes": system.used_swap(),
            "source": "native sysinfo memory API"
        },
        "gpu": {"available":false,"reason":"native GPU counters unavailable on this platform"},
        "thermal": {"available":false,"reason":"native thermal state unavailable on this platform"},
        "power": {"available":false,"reason":"fixed powermetrics sampling requires separately authorized system access"}
    });
    #[cfg(target_os = "macos")]
    {
        match crate::bounded_command(
            "/usr/sbin/ioreg",
            &[
                "-r".into(),
                "-c".into(),
                "AGXAccelerator".into(),
                "-l".into(),
                "-w".into(),
                "0".into(),
            ],
            std::time::Duration::from_secs(2),
            512 * 1024,
        ) {
            Ok(output) if output.success => {
                let mut counters = serde_json::Map::new();
                for (native, field) in [
                    ("Device Utilization %", "device_utilization_percent"),
                    ("Renderer Utilization %", "renderer_utilization_percent"),
                    ("Tiler Utilization %", "tiler_utilization_percent"),
                    ("Alloc system memory", "allocated_system_memory_bytes"),
                    ("In use system memory", "in_use_system_memory_bytes"),
                    ("recoveryCount", "recovery_count"),
                ] {
                    if let Some(number) = ioreg_number(&output.stdout, native) {
                        counters.insert(field.into(), number);
                    }
                }
                value["gpu"] = json!({"available":!counters.is_empty(),"counters":counters,"source":"IORegistry AGXAccelerator PerformanceStatistics","scope":"device-wide reported counters"});
            }
            Ok(output) => value["gpu"] = json!({"available":false,"reason":output.stderr}),
            Err(error) => value["gpu"] = json!({"available":false,"reason":error.to_string()}),
        }
        match crate::bounded_command(
            "/usr/bin/pmset",
            &["-g".into(), "therm".into()],
            std::time::Duration::from_secs(2),
            8192,
        ) {
            Ok(output) if output.success => {
                value["thermal"] = json!({"available":true,"state":output.stdout,"source":"pmset -g therm","scope":"system thermal warnings and performance limits"})
            }
            Ok(output) => value["thermal"] = json!({"available":false,"reason":output.stderr}),
            Err(error) => value["thermal"] = json!({"available":false,"reason":error.to_string()}),
        }
    }
    Ok(object(
        "metrics",
        &json!({"host":sysinfo::System::host_name().unwrap_or_else(|| "local".into())}),
        value,
        crate::now_ms(),
        "native system metrics",
        None,
        "host",
    ))
}

#[cfg(any(test, target_os = "macos"))]
fn ioreg_number(output: &str, key: &str) -> Option<Value> {
    let marker = format!("\"{key}\"");
    let tail = output.split_once(&marker)?.1.trim_start();
    let tail = tail.strip_prefix('=')?.trim_start();
    let length = tail
        .bytes()
        .take_while(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'-'))
        .count();
    if length == 0 {
        return None;
    }
    serde_json::from_str(&tail[..length])
        .ok()
        .filter(Value::is_number)
}

#[derive(Clone, Debug)]
pub struct SubscriptionOptions {
    pub topic: String,
    pub pid: Option<u32>,
    pub directory: Option<std::path::PathBuf>,
    pub seconds: Option<u64>,
    pub limit: Option<usize>,
    pub interval_ms: u64,
}

impl SubscriptionOptions {
    pub fn parse(args: &[String]) -> Result<Self> {
        let topic = args
            .first()
            .context("sys.subscribe requires process|files|network")?;
        ensure!(
            ["process", "files", "network"].contains(&topic.as_str()),
            "unknown system event topic"
        );
        let mut options = Self {
            topic: topic.clone(),
            pid: None,
            directory: None,
            seconds: None,
            limit: None,
            interval_ms: 1000,
        };
        let mut arguments = args[1..].iter();
        while let Some(option) = arguments.next() {
            let value = arguments
                .next()
                .context("subscription option requires a value")?;
            match option.as_str() {
                "--pid" => options.pid = Some(value.parse().context("invalid process ID")?),
                "--directory" => options.directory = Some(value.into()),
                "--seconds" => {
                    options.seconds = Some(value.parse().context("invalid subscription duration")?)
                }
                "--limit" => {
                    options.limit = Some(value.parse().context("invalid subscription event limit")?)
                }
                "--interval-ms" => {
                    options.interval_ms =
                        value.parse().context("invalid network sample interval")?
                }
                _ => bail!("unknown subscription option {option}"),
            }
        }
        ensure!(
            options
                .seconds
                .is_none_or(|value| (1..=86400).contains(&value)),
            "subscription duration outside 1..86400 seconds"
        );
        ensure!(
            options
                .limit
                .is_none_or(|value| (1..=100000).contains(&value)),
            "subscription limit outside 1..100000 records"
        );
        ensure!(
            (1000..=60000).contains(&options.interval_ms),
            "network interval outside 1000..60000 ms"
        );
        if options.topic == "process" {
            ensure!(
                options.pid.is_some_and(|pid| pid > 0),
                "process subscription requires --pid PID"
            );
        }
        if options.topic == "files" {
            ensure!(
                options.directory.is_some(),
                "files subscription requires --directory PATH"
            );
        }
        Ok(options)
    }
}

struct NativeEvent {
    event: &'static str,
    source: String,
    at_ms: u64,
    data: Value,
}

enum EventSource {
    Process {
        observation: crate::ProcessObservation,
        wake: std::sync::mpsc::Receiver<()>,
        identity: crate::ProcessIdentity,
    },
    Files {
        _watcher: notify::RecommendedWatcher,
        events: std::sync::mpsc::Receiver<NativeEvent>,
        dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    },
    Network {
        interval: std::time::Duration,
        next_sample: std::time::Instant,
    },
}

/// An explicitly requested native subscription. Queues are bounded independently
/// of the pipe consumer. Loss is reported before the next available event.
pub struct EventSubscription {
    id: String,
    topic: String,
    source: EventSource,
    pending: std::collections::VecDeque<NativeEvent>,
    sequence: u64,
    dropped_seen: u64,
}

impl EventSubscription {
    pub fn start(client: &SystemClient, options: &SubscriptionOptions) -> Result<Self> {
        use notify::Watcher;
        use std::sync::{
            atomic::{AtomicU64, Ordering},
            mpsc, Arc,
        };
        let source = match options.topic.as_str() {
            "process" => {
                let identity = crate::process_identity(options.pid.context("missing process ID")?)?;
                let (sender, wake) = mpsc::sync_channel(1);
                let directory = options
                    .directory
                    .as_deref()
                    .unwrap_or_else(|| client.managed_root());
                let observation = crate::ProcessObservation::start_with_duration(
                    identity.clone(),
                    directory,
                    options.seconds.map(std::time::Duration::from_secs),
                    move || {
                        let _ = sender.try_send(());
                    },
                )?;
                EventSource::Process {
                    observation,
                    wake,
                    identity,
                }
            }
            "files" => {
                let directory = options
                    .directory
                    .as_ref()
                    .context("missing observation directory")?
                    .canonicalize()
                    .context("file observation directory unavailable")?;
                ensure!(
                    directory.is_dir(),
                    "file observation path must be a directory"
                );
                let (sender, events) = mpsc::sync_channel(128);
                let dropped = Arc::new(AtomicU64::new(0));
                let overflow = dropped.clone();
                let mut watcher = notify::recommended_watcher(
                    move |event: notify::Result<notify::Event>| {
                        let value = match event {
                            Ok(event) => NativeEvent {
                                event: match event.kind {
                                    notify::EventKind::Create(_) => "file.created",
                                    notify::EventKind::Modify(_) => "file.modified",
                                    notify::EventKind::Remove(_) => "file.removed",
                                    notify::EventKind::Access(_) => "file.accessed",
                                    _ => "file.changed",
                                },
                                source: "native filesystem notification".into(),
                                at_ms: crate::now_ms(),
                                data: json!({"native_kind":format!("{:?}", event.kind),"paths":event.paths.iter().take(16).map(|path| path.to_string_lossy().chars().take(1024).collect::<String>()).collect::<Vec<_>>(),"recursive":false,"path_count":event.paths.len(),"paths_truncated":event.paths.len()>16}),
                            },
                            Err(error) => NativeEvent {
                                event: "subscription.error",
                                source: "native filesystem notification".into(),
                                at_ms: crate::now_ms(),
                                data: json!({"error":error.to_string()}),
                            },
                        };
                        if sender.try_send(value).is_err() {
                            overflow.fetch_add(1, Ordering::Relaxed);
                        }
                    },
                )?;
                watcher.watch(&directory, notify::RecursiveMode::NonRecursive)?;
                EventSource::Files {
                    _watcher: watcher,
                    events,
                    dropped,
                }
            }
            "network" => EventSource::Network {
                interval: std::time::Duration::from_millis(options.interval_ms),
                next_sample: std::time::Instant::now(),
            },
            _ => bail!("unknown event subscription topic"),
        };
        Ok(Self {
            id: crate::random_token()?,
            topic: options.topic.clone(),
            source,
            pending: Default::default(),
            sequence: 0,
            dropped_seen: 0,
        })
    }

    pub fn ready(&self) -> Value {
        json!({"schema":"glass.system.event.v1","kind":"system.event","subscription_id":self.id,"sequence":0,"topic":self.topic,"event":"subscription.ready","sampled_at_ms":crate::now_ms(),"queue_policy":"bounded; report loss","process_queue_limit":1024,"file_queue_limit":128,"network_source":"native cumulative interface samples"})
    }

    pub fn finished(&self, records: usize) -> Value {
        json!({"schema":"glass.system.event.v1","kind":"system.event","subscription_id":self.id,"sequence":self.sequence+1,"topic":self.topic,"event":"subscription.completed","sampled_at_ms":crate::now_ms(),"records":records})
    }

    fn loss(&mut self, total: u64) {
        if total > self.dropped_seen {
            self.pending.push_back(NativeEvent {
                event: "subscription.loss",
                source: "bounded native event queue".into(),
                at_ms: crate::now_ms(),
                data: json!({"dropped":total-self.dropped_seen,"dropped_total":total}),
            });
            self.dropped_seen = total;
        }
    }

    fn record(&mut self, event: NativeEvent) -> Result<Value> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("subscription sequence exhausted")?;
        Ok(
            json!({"schema":"glass.system.event.v1","kind":"system.event","subscription_id":self.id,"sequence":self.sequence,"topic":self.topic,"event":event.event,"sampled_at_ms":event.at_ms,"source":event.source,"data":event.data}),
        )
    }

    pub fn next(&mut self, deadline: Option<std::time::Instant>) -> Result<Option<Value>> {
        use std::sync::{atomic::Ordering, mpsc};
        use std::time::Instant;
        loop {
            if let Some(event) = self.pending.pop_front() {
                return self.record(event).map(Some);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(None);
            }
            match &self.source {
                EventSource::Process {
                    observation,
                    wake,
                    identity,
                } => {
                    let (events, dropped) = observation.take_events();
                    let identity = serde_json::to_value(identity)?;
                    let active = observation.active();
                    let values: Vec<_> = events
                        .into_iter()
                        .map(|event| NativeEvent {
                            event: if event.message.starts_with("Kernel exit notification") {
                                "process.exited"
                            } else if event.message.starts_with("Kernel fork notification") {
                                "process.forked"
                            } else if event.message.starts_with("Kernel exec notification") {
                                "process.executed"
                            } else {
                                "process.observation"
                            },
                            source: event.source,
                            at_ms: event.at_ms,
                            data: json!({"identity":identity,"message":event.message}),
                        })
                        .collect();
                    if !values.is_empty() || dropped > self.dropped_seen {
                        self.loss(dropped);
                        self.pending.extend(values);
                        continue;
                    }
                    if !active {
                        return Ok(None);
                    }
                    let waited = if let Some(deadline) = deadline {
                        wake.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                            .map_err(|error| match error {
                                mpsc::RecvTimeoutError::Timeout => false,
                                mpsc::RecvTimeoutError::Disconnected => true,
                            })
                    } else {
                        wake.recv().map_err(|_| true)
                    };
                    match waited {
                        Ok(()) => {}
                        Err(false) => return Ok(None),
                        Err(true) => bail!("native process event wakeup disconnected"),
                    }
                }
                EventSource::Files {
                    events, dropped, ..
                } => {
                    let total = dropped.load(Ordering::Relaxed);
                    if total > self.dropped_seen {
                        self.loss(total);
                        continue;
                    }
                    let received = if let Some(deadline) = deadline {
                        events
                            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                            .map_err(|error| match error {
                                mpsc::RecvTimeoutError::Timeout => false,
                                mpsc::RecvTimeoutError::Disconnected => true,
                            })
                    } else {
                        events.recv().map_err(|_| true)
                    };
                    match received {
                        Ok(event) => self.pending.push_back(event),
                        Err(false) => return Ok(None),
                        Err(true) => bail!("native file event source disconnected"),
                    }
                }
                EventSource::Network {
                    interval,
                    next_sample,
                } => {
                    if Instant::now() < *next_sample {
                        let until =
                            deadline.map_or(*next_sample, |deadline| deadline.min(*next_sample));
                        std::thread::sleep(until.saturating_duration_since(Instant::now()));
                        continue;
                    }
                    let interval = *interval;
                    let networks = sysinfo::Networks::new_with_refreshed_list();
                    for (name, counters) in &networks {
                        self.pending.push_back(NativeEvent { event: "network.sample", source: "native cumulative interface counters".into(), at_ms: crate::now_ms(), data: json!({"name":name,"received_bytes":counters.total_received(),"transmitted_bytes":counters.total_transmitted(),"sample_kind":"cumulative-interface-counters"}) });
                    }
                    if let EventSource::Network { next_sample, .. } = &mut self.source {
                        *next_sample = Instant::now() + interval;
                    }
                }
            }
        }
    }
}

// FVPROV1 43707 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiM2I3OWU5Y2NkYTVlOThmOWZhMzY2Y2YyYjg5Yjc2MWZkMmNmNTA4YTI2NDA1Y2JjNzc5ZmQ1ZjJlZDBmNDI5YSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiYTA4OGExNWJjNDAzYjJlMzZhYWEyYmIwNGMwZDhkOGE2ZGIzMGI5NGRiMGI1NmFkMTRlYTA3YTQ1NTBhMmRlNTY0YjRjNWI3NDc4ZmU2ZTExMWI3Mzc3ODI5NTU0Y2E2NDI2ZWE5ZTQ2NzRkNWI2NTlmNjAxOWRiMDQ1YmZkMGQifV19XQ== FVPROV1-END

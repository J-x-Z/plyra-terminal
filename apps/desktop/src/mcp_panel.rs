use crate::{config::Settings, Wake};
use anyhow::{Context, Result};
use egui::{RichText, TextEdit};
use mcp_client::{
    Cancellation, Client, McpReply, PromptInfo, ResourceInfo, ServerConfig, ToolInfo,
};
use serde_json::Value;
use std::{collections::BTreeMap, sync::mpsc, thread};
use winit::event_loop::EventLoopProxy;

type Outcome<T> = std::result::Result<T, String>;
enum Update {
    Connected(Outcome<Client>),
    Tools(Outcome<Vec<ToolInfo>>),
    Resources(Outcome<Vec<ResourceInfo>>),
    Prompts(Outcome<Vec<PromptInfo>>),
    Reply(Outcome<McpReply>),
    Disconnected(Outcome<()>),
}
struct Message {
    server_id: String,
    generation: u64,
    update: Update,
}

pub struct McpPanel {
    pub open: bool,
    proxy: EventLoopProxy<Wake>,
    sender: mpsc::Sender<Message>,
    receiver: mpsc::Receiver<Message>,
    clients: BTreeMap<String, Client>,
    selected: String,
    generation: u64,
    busy: bool,
    tools: Vec<ToolInfo>,
    resources: Vec<ResourceInfo>,
    prompts: Vec<PromptInfo>,
    tool_name: String,
    resource_uri: String,
    prompt_name: String,
    arguments: String,
    prompt_arguments: String,
    output: String,
    status: Option<String>,
    connect_cancel: Option<Cancellation>,
    retiring: Vec<thread::JoinHandle<()>>,
    requests: Vec<thread::JoinHandle<()>>,
}
impl McpPanel {
    pub fn new(proxy: EventLoopProxy<Wake>) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            open: false,
            proxy,
            sender,
            receiver,
            clients: BTreeMap::new(),
            selected: String::new(),
            generation: 0,
            busy: false,
            tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
            tool_name: String::new(),
            resource_uri: String::new(),
            prompt_name: String::new(),
            arguments: "{}".into(),
            prompt_arguments: "{}".into(),
            output: String::new(),
            status: None,
            connect_cancel: None,
            retiring: Vec::new(),
            requests: Vec::new(),
        }
    }
    pub fn dismiss(&mut self) {
        self.open = false;
        self.cancel_connect();
        for client in self.clients.values() {
            client.revoke_pending_calls();
            client.cancel_active_requests();
        }
    }
    pub fn reconfigure(&mut self) {
        self.retire_connect();
        self.generation = self.generation.wrapping_add(1);
        self.busy = false;
        self.clear_lists();
        for (_, client) in std::mem::take(&mut self.clients) {
            self.retire_client(client);
        }
        self.status =
            Some("Configuration changed. Connect a server to use its current settings.".into());
    }
    fn cancel_connect(&mut self) {
        if self.connect_cancel.is_some() {
            self.generation = self.generation.wrapping_add(1);
            self.busy = false;
            self.retire_connect();
            self.status = Some("Connection cancelled".into());
        }
    }
    fn retire_connect(&mut self) {
        if let Some(cancel) = self.connect_cancel.take() {
            cancel.cancel();
            self.retiring.push(thread::spawn(move || {
                if cancel.cancel_and_wait().is_err() {
                    eprintln!("MCP connection cleanup failed");
                }
            }));
        }
    }
    fn retire_client(&mut self, client: Client) {
        self.retiring.push(thread::spawn(move || {
            if client.disconnect().is_err() {
                eprintln!("MCP disconnect cleanup failed");
            }
        }));
    }
    pub fn shutdown(&mut self) {
        self.retire_connect();
        self.generation = self.generation.wrapping_add(1);
        for (_, client) in std::mem::take(&mut self.clients) {
            self.retire_client(client);
        }
        for worker in std::mem::take(&mut self.requests) {
            let _ = worker.join();
        }
        // All UI request producers have retired. Reclaim clients returned just before quit.
        while let Ok(message) = self.receiver.try_recv() {
            if let Update::Connected(Ok(client)) = message.update {
                self.retire_client(client);
            }
        }
        for worker in std::mem::take(&mut self.retiring) {
            let _ = worker.join();
        }
    }
    pub fn poll(&mut self) {
        for worker in std::mem::take(&mut self.requests) {
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                self.requests.push(worker);
            }
        }
        for worker in std::mem::take(&mut self.retiring) {
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                self.retiring.push(worker);
            }
        }
        while let Ok(message) = self.receiver.try_recv() {
            if message.generation != self.generation {
                if let Update::Connected(Ok(client)) = message.update {
                    self.retire_client(client);
                }
                continue;
            }
            self.busy = false;
            if matches!(&message.update, Update::Connected(Err(_))) {
                self.retire_connect();
            } else if matches!(&message.update, Update::Connected(Ok(_))) {
                self.connect_cancel = None;
            }
            match message.update {
                Update::Connected(result) => match result {
                    Ok(client) => {
                        let info = client.info();
                        self.status = Some(format!(
                            "Connected · {} {} · protocol {}",
                            info.server_name, info.server_version, info.protocol_version
                        ));
                        self.clients.insert(message.server_id, client);
                    }
                    Err(error) => self.status = Some(error),
                },
                Update::Tools(result) => match result {
                    Ok(items) => {
                        self.status = Some(format!("Listed {} tools", items.len()));
                        self.tools = items;
                    }
                    Err(error) => self.status = Some(error),
                },
                Update::Resources(result) => match result {
                    Ok(items) => {
                        self.status = Some(format!("Listed {} resources", items.len()));
                        self.resources = items;
                    }
                    Err(error) => self.status = Some(error),
                },
                Update::Prompts(result) => match result {
                    Ok(items) => {
                        self.status = Some(format!("Listed {} prompts", items.len()));
                        self.prompts = items;
                    }
                    Err(error) => self.status = Some(error),
                },
                Update::Reply(result) => match result {
                    Ok(reply) => {
                        self.status = Some(format!(
                            "{} · {} · {} ms",
                            reply.server_id, reply.method, reply.elapsed_ms
                        ));
                        self.output = serde_json::to_string_pretty(&reply.data)
                            .unwrap_or_else(|error| error.to_string());
                    }
                    Err(error) => self.status = Some(error),
                },
                Update::Disconnected(result) => {
                    self.clients.remove(&message.server_id);
                    self.status = Some(match result {
                        Ok(()) => "Disconnected".into(),
                        Err(error) => error,
                    });
                    self.clear_lists();
                }
            }
        }
    }
    fn clear_lists(&mut self) {
        self.tools.clear();
        self.resources.clear();
        self.prompts.clear();
        self.tool_name.clear();
        self.resource_uri.clear();
        self.prompt_name.clear();
        self.output.clear();
    }
    fn start(&mut self, server_id: String, work: impl FnOnce() -> Update + Send + 'static) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some("Request in progress".into());
        let sender = self.sender.clone();
        let proxy = self.proxy.clone();
        let generation = self.generation;
        self.requests.push(thread::spawn(move || {
            let update = work();
            let _ = sender.send(Message {
                server_id,
                generation,
                update,
            });
            let _ = proxy.send_event(Wake::System);
        }));
    }
    pub fn show(&mut self, ctx: &egui::Context, settings: &Settings) {
        self.poll();
        if !self.open {
            return;
        }
        let mut open = true;
        egui::Window::new("MCP")
            .open(&mut open)
            .default_size([700.0, 620.0])
            .min_width((ctx.screen_rect().width() - 32.0).clamp(80.0, 420.0))
            .max_width((ctx.screen_rect().width() - 32.0).max(80.0))
            .max_height((ctx.screen_rect().height() - 60.0).max(80.0))
            .vscroll(true)
            .show(ctx, |ui| {
                if settings.mcp_servers.is_empty() {
                    ui.label(
                        "Add a server to mcp_servers in config.json, then Reload configuration.",
                    );
                    ui.small(Settings::path().display().to_string());
                    return;
                }
                if !settings
                    .mcp_servers
                    .iter()
                    .any(|server| server.id == self.selected)
                {
                    self.selected = settings.mcp_servers[0].id.clone();
                    self.clear_lists();
                }
                let previous = self.selected.clone();
                ui.add_enabled_ui(!self.busy, |ui| {
                    egui::ComboBox::from_label("Server")
                        .selected_text(&self.selected)
                        .show_ui(ui, |ui| {
                            for server in &settings.mcp_servers {
                                ui.selectable_value(
                                    &mut self.selected,
                                    server.id.clone(),
                                    format!("{} · {}", server.id, server.name),
                                );
                            }
                        });
                });
                if previous != self.selected {
                    self.clear_lists();
                }
                let server = settings
                    .mcp_servers
                    .iter()
                    .find(|server| server.id == self.selected)
                    .expect("selected MCP server");
                let client = self.clients.get(&self.selected).cloned();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.busy && client.is_none() && server.enabled,
                            egui::Button::new("Connect"),
                        )
                        .clicked()
                    {
                        self.connect(server.clone());
                    }
                    if ui
                        .add_enabled(
                            !self.busy && client.is_some(),
                            egui::Button::new("Disconnect"),
                        )
                        .clicked()
                    {
                        let client = client.clone().unwrap();
                        self.start(self.selected.clone(), move || {
                            Update::Disconnected(outcome(client.disconnect()))
                        });
                    }
                    if ui
                        .add_enabled(
                            self.busy,
                            egui::Button::new(if self.connect_cancel.is_some() {
                                "Cancel connection"
                            } else {
                                "Cancel request"
                            }),
                        )
                        .clicked()
                    {
                        if self.connect_cancel.is_some() {
                            self.cancel_connect();
                        } else if let Some(client) = &client {
                            client.cancel_active_requests();
                        }
                    }
                });
                if !server.enabled {
                    ui.label("This server is disabled in config.json.");
                }
                if let Some(status) = &self.status {
                    ui.label(status);
                }
                if let Some(client) = &client {
                    let info = client.info();
                    ui.small(format!(
                        "{} · {} · initialized {} ms since UNIX epoch",
                        info.transport, info.protocol_version, info.connected_at_ms
                    ));
                    egui::ScrollArea::vertical()
                        .max_height(500.0)
                        .show(ui, |ui| {
                            ui.collapsing("Tools", |ui| self.tool_ui(ui, client));
                            ui.collapsing("Resources", |ui| self.resource_ui(ui, client));
                            ui.collapsing("Prompts", |ui| self.prompt_ui(ui, client));
                            if !self.output.is_empty() {
                                ui.separator();
                                ui.label(RichText::new("Result").strong());
                                let mut output = self.output.as_str();
                                ui.add(
                                    TextEdit::multiline(&mut output)
                                        .font(egui::TextStyle::Monospace)
                                        .desired_width(f32::INFINITY)
                                        .desired_rows(12),
                                );
                            }
                        });
                }
            });
        if !open {
            self.dismiss();
        }
    }
    fn connect(&mut self, server: ServerConfig) {
        let cancellation = Cancellation::default();
        self.connect_cancel = Some(cancellation.clone());
        self.start(server.id.clone(), move || {
            Update::Connected(outcome(Client::connect_with_cancel(server, cancellation)))
        });
    }
    fn tool_ui(&mut self, ui: &mut egui::Ui, client: &Client) {
        if ui
            .add_enabled(!self.busy, egui::Button::new("List tools"))
            .clicked()
        {
            let client = client.clone();
            self.start(self.selected.clone(), move || {
                Update::Tools(outcome(client.list_tools()))
            });
        }
        egui::ComboBox::from_label("Tool")
            .selected_text(&self.tool_name)
            .show_ui(ui, |ui| {
                for tool in &self.tools {
                    ui.selectable_value(&mut self.tool_name, tool.name.clone(), &tool.name);
                }
            });
        if let Some(tool) = self.tools.iter().find(|tool| tool.name == self.tool_name) {
            if let Some(description) = &tool.description {
                ui.label(description);
            }
            ui.collapsing("Input schema", |ui| {
                ui.monospace(serde_json::to_string_pretty(&tool.input_schema).unwrap_or_default());
            });
        }
        ui.label("Arguments · JSON object");
        ui.add(
            TextEdit::multiline(&mut self.arguments)
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY)
                .desired_rows(4)
                .char_limit(64 * 1024),
        );
        if ui
            .add_enabled(
                !self.busy && !self.tool_name.is_empty(),
                egui::Button::new("Call tool"),
            )
            .clicked()
        {
            match object(&self.arguments) {
                Ok(arguments) => {
                    let client = client.clone();
                    let name = self.tool_name.clone();
                    self.start(self.selected.clone(), move || {
                        Update::Reply(outcome((|| {
                            let prepared = client.prepare_tool_call(&name, &arguments)?;
                            client.call_tool(&prepared.ticket)
                        })()))
                    });
                }
                Err(error) => self.status = Some(error.to_string()),
            }
        }
    }
    fn resource_ui(&mut self, ui: &mut egui::Ui, client: &Client) {
        if ui
            .add_enabled(!self.busy, egui::Button::new("List resources"))
            .clicked()
        {
            let client = client.clone();
            self.start(self.selected.clone(), move || {
                Update::Resources(outcome(client.list_resources()))
            });
        }
        egui::ComboBox::from_label("Resource")
            .selected_text(&self.resource_uri)
            .show_ui(ui, |ui| {
                for resource in &self.resources {
                    ui.selectable_value(
                        &mut self.resource_uri,
                        resource.uri.clone(),
                        &resource.name,
                    );
                }
            });
        ui.add(
            TextEdit::singleline(&mut self.resource_uri)
                .hint_text("Resource URI")
                .char_limit(8192)
                .desired_width(f32::INFINITY),
        );
        if ui
            .add_enabled(
                !self.busy && !self.resource_uri.is_empty(),
                egui::Button::new("Read resource"),
            )
            .clicked()
        {
            let client = client.clone();
            let uri = self.resource_uri.clone();
            self.start(self.selected.clone(), move || {
                Update::Reply(outcome(client.read_resource(&uri)))
            });
        }
    }
    fn prompt_ui(&mut self, ui: &mut egui::Ui, client: &Client) {
        if ui
            .add_enabled(!self.busy, egui::Button::new("List prompts"))
            .clicked()
        {
            let client = client.clone();
            self.start(self.selected.clone(), move || {
                Update::Prompts(outcome(client.list_prompts()))
            });
        }
        egui::ComboBox::from_label("Prompt")
            .selected_text(&self.prompt_name)
            .show_ui(ui, |ui| {
                for prompt in &self.prompts {
                    ui.selectable_value(&mut self.prompt_name, prompt.name.clone(), &prompt.name);
                }
            });
        if let Some(prompt) = self
            .prompts
            .iter()
            .find(|prompt| prompt.name == self.prompt_name)
        {
            if let Some(description) = &prompt.description {
                ui.label(description);
            }
            for argument in &prompt.arguments {
                ui.small(format!(
                    "{}{}",
                    argument.name,
                    if argument.required { " (required)" } else { "" }
                ));
            }
        }
        ui.label("Arguments · JSON object with string values");
        ui.add(
            TextEdit::multiline(&mut self.prompt_arguments)
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY)
                .desired_rows(3)
                .char_limit(64 * 1024),
        );
        if ui
            .add_enabled(
                !self.busy && !self.prompt_name.is_empty(),
                egui::Button::new("Get prompt"),
            )
            .clicked()
        {
            match object(&self.prompt_arguments) {
                Ok(arguments) => {
                    let client = client.clone();
                    let name = self.prompt_name.clone();
                    self.start(self.selected.clone(), move || {
                        Update::Reply(outcome(client.get_prompt(&name, &arguments)))
                    });
                }
                Err(error) => self.status = Some(error.to_string()),
            }
        }
    }
}
impl Drop for McpPanel {
    fn drop(&mut self) {
        self.shutdown();
    }
}
fn object(text: &str) -> Result<Value> {
    anyhow::ensure!(text.len() <= 64 * 1024, "Arguments exceed 64 KiB");
    let value: Value = serde_json::from_str(text).context("Arguments must use valid JSON")?;
    anyhow::ensure!(value.is_object(), "Arguments must be a JSON object");
    Ok(value)
}
fn outcome<T>(value: Result<T>) -> Outcome<T> {
    value.map_err(|error| format!("{error:#}"))
}

// FVPROV1 21055 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYjRjNWM5Y2JjNDRkNTlkMWEzZWIzOWFkZjQ2MDE5Y2E1ZWRhNWExYmQ3NDFkZTY3OTMwNzFmOWE1OTY3YmNkZSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNDFiMDFhMjUwNzM1NjIxZDQxYmM3MWJiYTcxNmI2OTYwOTNjNzBiMzA5MjBjMThmMTI0OGZkNjU4ZjM1OWE0NjI5NzA4ZDNjMzBkMDQ0YzcxOWM2YmVhNTcwZTNjNTdhNDJhYTIxZjJiYzM5OTY1OGNiZjkwOTY5MzUyNzk2MGQifV19XQ== FVPROV1-END

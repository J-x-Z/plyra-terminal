use crate::Wake;
use egui::{RichText, Ui};
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
};
use system_api::{ManagedFile, PluginCapabilities, PluginRun, RecoveryRecord, SystemClient};
use winit::event_loop::EventLoopProxy;

enum Update {
    Files {
        root: String,
        files: Vec<ManagedFile>,
        records: Vec<RecoveryRecord>,
        message: Option<String>,
    },
    Read(String, String),
    Plugin(PluginRun),
    Error(String),
}

pub struct Extensions {
    pub plugin_page: bool,
    root: String,
    files: Vec<ManagedFile>,
    records: Vec<RecoveryRecord>,
    filename: String,
    contents: String,
    plugin_program: String,
    plugin_args: String,
    write: bool,
    network: bool,
    run: Option<PluginRun>,
    busy: bool,
    status: Option<String>,
    restore: Option<RecoveryRecord>,
    tx: Sender<Update>,
    rx: Receiver<Update>,
    proxy: EventLoopProxy<Wake>,
}

impl Extensions {
    pub fn dismiss_dialogs(&mut self) {
        self.restore = None;
    }
    pub fn blocks_terminal(&self) -> bool {
        self.restore.is_some()
    }
    pub fn new(proxy: EventLoopProxy<Wake>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            plugin_page: false,
            root: String::new(),
            files: Vec::new(),
            records: Vec::new(),
            filename: String::new(),
            contents: String::new(),
            plugin_program: String::new(),
            plugin_args: String::new(),
            write: false,
            network: false,
            run: None,
            busy: false,
            status: None,
            restore: None,
            tx,
            rx,
            proxy,
        }
    }
    fn worker(&mut self, job: impl FnOnce() -> anyhow::Result<Update> + Send + 'static) {
        if self.busy {
            return;
        }
        self.busy = true;
        let tx = self.tx.clone();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = job().unwrap_or_else(|error| Update::Error(error.to_string()));
            let _ = tx.send(result);
            let _ = proxy.send_event(Wake::System);
        });
    }
    fn listing(client: &SystemClient, message: Option<String>) -> anyhow::Result<Update> {
        Ok(Update::Files {
            root: client.managed_root().display().to_string(),
            files: client.managed_files()?,
            records: client.recovery_records()?,
            message,
        })
    }
    pub fn refresh(&mut self) {
        self.worker(|| Self::listing(&SystemClient::new()?, None));
    }
    pub fn receive(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        while let Ok(update) = self.rx.try_recv() {
            self.busy = false;
            match update {
                Update::Files {
                    root,
                    files,
                    records,
                    message,
                } => {
                    self.root = root;
                    self.files = files;
                    self.records = records;
                    self.status = message.clone();
                    if let Some(message) = message {
                        events.push(message);
                    }
                }
                Update::Read(name, text) => {
                    self.filename = name;
                    self.contents = text;
                    self.status = None;
                }
                Update::Plugin(run) => {
                    events.push(format!(
                        "Plugin {} exited {:?} · {}",
                        run.program, run.exit_code, run.sandbox
                    ));
                    self.run = Some(run);
                    self.status = None;
                }
                Update::Error(error) => {
                    events.push(format!("Extension error · {error}"));
                    self.status = Some(error);
                }
            }
        }
        events
    }
    pub fn show(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.busy, egui::Button::new("Refresh"))
                .clicked()
            {
                self.refresh();
            }
        });
        if self.busy {
            ui.label("Working");
        } else if let Some(status) = &self.status {
            ui.colored_label(egui::Color32::from_rgb(245, 187, 120), status);
        }
        if self.plugin_page {
            self.plugin_ui(ui);
        } else {
            self.recovery_ui(ui);
        }
    }
    fn recovery_ui(&mut self, ui: &mut Ui) {
        ui.small(format!("Managed directory · {}", self.root));
        let mut read = None;
        let mut restore = None;
        egui::ScrollArea::vertical().id_salt("recovery").show(ui,|ui|{
            ui.horizontal_wrapped(|ui|for file in &self.files{if ui.button(format!("{} · {} bytes",file.path,file.bytes)).clicked(){read=Some(file.path.clone());}});
            ui.label("File name");ui.add(egui::TextEdit::singleline(&mut self.filename).char_limit(255));
            ui.add(egui::TextEdit::multiline(&mut self.contents).code_editor().desired_width(f32::INFINITY).desired_rows(10).char_limit(1_048_576));
            if ui.add_enabled(!self.busy&&!self.filename.is_empty(),egui::Button::new("Save tracked change")).clicked(){
                let path=self.filename.clone();let contents=self.contents.clone();self.worker(move||{let client=SystemClient::new()?;let record=client.edit_managed(&path,&contents)?;Self::listing(&client,Some(format!("Tracked change saved · {} · {}",record.path,record.id))) });
            }
            ui.separator();ui.label(RichText::new("Recovery transactions").strong());
            for record in self.records.iter().rev(){
                ui.horizontal(|ui|{ui.label(&record.path);ui.small(&record.status);
                    if ui.add_enabled(!self.busy&&(record.status=="applied"||record.status=="prepared"),egui::Button::new("Restore").small()).clicked(){restore=Some(record.clone());}});
                ui.small(format!("{} · {}",record.created_at_ms,record.id));
            }
            ui.small("Recovery covers tracked changes in this directory. Hash checks detect changes made after a transaction.");
        });
        if let Some(path) = read {
            self.worker(move || {
                let client = SystemClient::new()?;
                let contents = client.read_managed(&path)?;
                Ok(Update::Read(path, contents))
            });
        }
        if let Some(record) = restore {
            self.restore = Some(record);
        }
    }
    fn plugin_ui(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical().id_salt("plugins").show(ui,|ui|{
            ui.label("Program (absolute path)");ui.add(egui::TextEdit::singleline(&mut self.plugin_program).desired_width(f32::INFINITY).char_limit(4096));
            ui.label("Arguments (one per line)");ui.add(egui::TextEdit::multiline(&mut self.plugin_args).desired_rows(4).desired_width(f32::INFINITY).char_limit(16384));
            ui.checkbox(&mut self.write,"Allow writes to managed directory");ui.checkbox(&mut self.network,"Allow network access");
            if ui.add_enabled(!self.busy&&!self.plugin_program.is_empty(),egui::Button::new("Run sandboxed plugin")).clicked(){
                let program=PathBuf::from(self.plugin_program.trim());let args=self.plugin_args.lines().map(str::to_owned).collect::<Vec<_>>();let capabilities=PluginCapabilities{write_workspace:self.write,network:self.network};
                self.worker(move||{let client=SystemClient::new()?;Ok(Update::Plugin(client.run_plugin(&program,&args,capabilities)?))});
            }
            ui.small("Runs in the platform sandbox for at most 3 seconds. Broker access remains unavailable to plugins.");
            if let Some(run)=&self.run{
                ui.separator();ui.small(format!("Source {} · timestamp {} · exit {:?}",run.sandbox,run.started_at_ms,run.exit_code));
                if ui.button("Copy output").clicked(){ui.ctx().copy_text(run.output.clone());}
                ui.monospace(&run.output);
            }
        });
    }
    pub fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(record) = self.restore.clone() {
            let mut confirm = false;
            let mut cancel = false;
            egui::Window::new("Restore tracked file")
                .collapsible(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Restore {} to its state before this transaction?",
                        record.path
                    ));
                    ui.small(&record.id);
                    ui.horizontal(|ui| {
                        confirm = ui.button("Restore file").clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            if cancel {
                self.restore = None;
            }
            if confirm {
                self.restore = None;
                self.worker(move || {
                    let client = SystemClient::new()?;
                    let restored = client.rollback(&record.id)?;
                    Self::listing(
                        &client,
                        Some(format!("Restored tracked file · {}", restored.path)),
                    )
                });
            }
        }
    }
}

// FVPROV1 9567 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNDRiNmI0NzQ3ZTg2M2E3ZGM2OTVjZWQ2ZmNjMzZjOWI2NWMxMTg2MmJjZjRhODkxODViM2JiNWYwNTVjOTExYSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiNDZlMWMxMmY2OWQyOTRiOWM3MTNmNDc5MjdjNzIyMTY1MjIyZTM4MTBlMDkzZDc3ZDg5ZDY4OWYyYzhmMWIyZDQ2ZTk5MGI4OWE0ZmQyNWM5MTUyMTcyY2I2MGY4YzgyNjNmZjE4YzU4YWI3MjVlZmQ3NzJkZWFlN2YyMjBiMDEifV19XQ== FVPROV1-END

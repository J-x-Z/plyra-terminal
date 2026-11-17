use super::*;

pub(super) struct Probe {
    pub(super) dir: PathBuf,
    pub(super) started: Instant,
    pub(super) phase_started: Instant,
    pub(super) phase: u8,
    pub(super) ready: bool,
    pub(super) helper_pid: Option<u32>,
    pub(super) shell_pid: Option<u32>,
    pub(super) exit_code: Option<i32>,
    pub(super) initial_dimensions: (u16, u16),
    resize_observed: bool,
    unverified_checks: Vec<String>,
    pub(super) idle_frames: u64,
    idle_observation: Option<serde_json::Value>,
    idle_observations: Vec<serde_json::Value>,
    idle_surface_before: Option<String>,
    native_mode_observations: Vec<serde_json::Value>,
    gpu_timing: Option<serde_json::Value>,
    initial_capture: bool,
    pub(super) checks: Vec<serde_json::Value>,
}

impl Probe {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            started: Instant::now(),
            phase_started: Instant::now(),
            phase: 0,
            ready: false,
            helper_pid: None,
            shell_pid: None,
            exit_code: None,
            initial_dimensions: (0, 0),
            resize_observed: false,
            unverified_checks: Vec::new(),
            idle_frames: 0,
            idle_observation: None,
            idle_observations: Vec::new(),
            idle_surface_before: None,
            native_mode_observations: Vec::new(),
            gpu_timing: None,
            initial_capture: false,
            checks: Vec::new(),
        }
    }

    fn next(&mut self) {
        self.phase += 1;
        self.phase_started = Instant::now();
    }

    fn check(&mut self, name: &str, passed: bool) -> Result<()> {
        self.checks
            .push(serde_json::json!({"case":name,"passed":passed}));
        anyhow::ensure!(passed, "native probe failed: {name}");
        Ok(())
    }

    fn begin_idle(&mut self, app: &App) {
        self.idle_frames = app.frames;
        self.idle_surface_before = app.renderer.as_ref().map(Renderer::diagnostics);
        self.next();
    }

    fn observe_idle(&mut self, app: &App, effect: &str) -> bool {
        let observation = serde_json::json!({
            "effect": effect,
            "elapsed_ms": self.phase_started.elapsed().as_millis(),
            "presented_frames_before": self.idle_frames,
            "presented_frames_after": app.frames,
            "presented_frame_delta": app.frames.saturating_sub(self.idle_frames),
            "diagnostics_before": self.idle_surface_before,
            "diagnostics_after": app.renderer.as_ref().map(Renderer::diagnostics),
            "source": "Application surface.present count during the native probe idle phase"
        });
        if self.idle_observation.is_none() {
            self.idle_observation = Some(observation.clone());
        }
        self.idle_observations.push(observation);
        app.frames == self.idle_frames
    }

    fn collect_gpu_timing(&mut self, app: &mut App) {
        let Some(renderer) = &mut app.renderer else {
            return;
        };
        let report = renderer.collect_gpu_timings();
        let mut milliseconds: Vec<_> = report
            .samples
            .iter()
            .map(|sample| sample.duration_ns / 1_000_000.0)
            .collect();
        milliseconds.sort_by(f64::total_cmp);
        let p50 = milliseconds
            .get((milliseconds.len() * 50).div_ceil(100).saturating_sub(1))
            .copied();
        let p95 = milliseconds
            .get((milliseconds.len() * 95).div_ceil(100).saturating_sub(1))
            .copied();
        let samples: Vec<_> = report
            .samples
            .iter()
            .map(|sample| {
                serde_json::json!({
                    "renderer_frame": sample.renderer_frame,
                    "requested_effect": format!("{:?}", sample.requested_effect),
                    "pass": sample.pass,
                    "duration_ns": sample.duration_ns,
                })
            })
            .collect();
        self.gpu_timing = Some(serde_json::json!({
            "status":report.status,"requested":report.requested,"supported":report.supported,
            "enabled":report.enabled,"error":report.error,
            "timestamp_period_ns":report.timestamp_period_ns,"max_frames":report.max_frames,
            "recorded_frames":report.recorded_frames,"invalid_samples":report.invalid_samples,
            "source":"wgpu TIMESTAMP_QUERY render-pass boundary hardware timestamps multiplied by Queue::get_timestamp_period",
            "scope":"this application's surface terminal and optional egui render passes",
            "excluded":"NSGlassEffectView/WindowServer composition, queue wait, presentation, texture uploads, screenshot passes, GPU utilization and power",
            "collection_phase":"one resolve/readback after both static idle observations; bounded two-second wait",
            "pass_interval_ms":{"count":milliseconds.len(),"p50":p50,"p95":p95},
            "percentile_method":"nearest-rank, pooled sampled render passes",
            "samples":samples
        }));
    }

    pub(super) fn tick(&mut self, app: &mut App, event_loop: &ActiveEventLoop) -> Result<()> {
        if !self.initial_capture && app.frames > 0 {
            let screen = app.active().screen.clone();
            if let Some(renderer) = &mut app.renderer {
                renderer.screenshot(&screen, self.dir.join("initial-gpu.png"))?;
            }
            std::fs::write(
                self.dir.join("initial-screen.json"),
                serde_json::to_vec_pretty(&screen)?,
            )?;
            self.initial_capture = true;
        }
        if self.started.elapsed() > Duration::from_secs(25) {
            self.write_result(app, false)?;
            anyhow::bail!("native probe timed out in phase {}", self.phase);
        }
        let text = app.active().screen.plain_text();
        match self.phase {
            0 if self.ready => {
                let setup: &[u8] = if cfg!(windows) {
                    b"@echo off\rchcp 65001 >nul\r"
                } else {
                    b"stty -echo\r"
                };
                app.active_mut()
                    .session
                    .as_mut()
                    .context("missing session")?
                    .input(setup)?;
                self.next();
            }
            1 if self.phase_started.elapsed() > Duration::from_millis(200) => {
                // Pass committed UTF-8 through the application's IME commit path.
                if cfg!(windows) {
                    app.commit("powershell.exe -NoProfile -Command \"[Console]::Write([string]([char]27)+'[2J'+[char]27+'[HGLASS_ASCII 中文 日本語 한국어 e\u{301}'+[char]10+[char]27+'[31mANSI_RED'+[char]27+'[0m'+[char]10)\"\r")?;
                } else {
                    app.commit("printf '\\033[2J\\033[HGLASS_ASCII ")?;
                    app.commit("中文 日本語 한국어 e\u{301}")?;
                    app.commit("\\n\\033[31mANSI_RED\\033[0m\\n'\r")?;
                }
                self.next();
            }
            2 if text.contains("GLASS_ASCII 中文 日本語 한국어 e\u{301}")
                && text.contains("ANSI_RED") =>
            {
                self.check("real PTY and committed UTF-8 output", true)?;
                let red_text = app
                    .active()
                    .screen
                    .lines
                    .iter()
                    .flat_map(|line| &line.runs)
                    .filter(|run| run.fg[0] > run.fg[1])
                    .map(|run| run.text.as_str())
                    .collect::<String>();
                self.check("ANSI foreground color", red_text.contains("ANSI_RED"))?;
                self.initial_dimensions = (app.active().screen.cols, app.active().screen.rows);
                if let Some(window) = &app.window {
                    let _ = window.request_inner_size(LogicalSize::new(820.0, 500.0));
                }
                self.next();
            }
            3 if (app.active().screen.cols, app.active().screen.rows)
                != self.initial_dimensions
                || (cfg!(target_os = "linux")
                    && self.phase_started.elapsed() > Duration::from_millis(1500)) =>
            {
                self.resize_observed =
                    (app.active().screen.cols, app.active().screen.rows) != self.initial_dimensions;
                if !self.resize_observed {
                    self.unverified_checks.push(
                        "The compositor did not apply the requested window size. Interactive window resizing requires a separate native check.".into(),
                    );
                }
                let command = if cfg!(windows) {
                    "powershell.exe -NoProfile -Command \"'SIZE_' + [Console]::WindowHeight + ' ' + [Console]::WindowWidth; 'RESIZE_OK'; Start-Sleep 20\"\r"
                } else {
                    "printf 'SIZE_'; stty size; printf 'RESIZE_OK\\n'; sleep 20\r"
                };
                app.active_mut()
                    .session
                    .as_mut()
                    .context("missing session")?
                    .input(command.as_bytes())?;
                self.next();
            }
            4 if text.contains("RESIZE_OK")
                && self.phase_started.elapsed() > Duration::from_millis(300) =>
            {
                self.check(
                    if self.resize_observed {
                        "native window resize reaches PTY grid"
                    } else {
                        "PTY dimensions match compositor-managed window"
                    },
                    text.contains(&format!(
                        "SIZE_{} {}",
                        app.active().screen.rows,
                        app.active().screen.cols
                    )),
                )?;
                app.active_mut()
                    .session
                    .as_mut()
                    .context("missing session")?
                    .input(&[3])?;
                self.next();
            }
            5 if self.phase_started.elapsed() > Duration::from_millis(300) => {
                let command: &[u8] = if cfg!(windows) {
                    b"echo INTERRUPT_^OK\r"
                } else {
                    b"printf '%s_%s\\n' INTERRUPT OK\r"
                };
                app.active_mut()
                    .session
                    .as_mut()
                    .context("missing session")?
                    .input(command)?;
                self.next();
            }
            6 if text.contains("INTERRUPT_OK") => {
                self.check(
                    "foreground interrupt completes within two seconds",
                    self.phase_started.elapsed() < Duration::from_secs(2),
                )?;
                self.check("Ctrl+C interrupts foreground sleep", true)?;
                let screen = app.active().screen.clone();
                let frames_before = app.renderer.as_ref().map_or(0, Renderer::frame_count);
                if let Some(renderer) = &mut app.renderer {
                    #[cfg(target_os = "macos")]
                    {
                        let off_before = renderer.diagnostics();
                        let size = app
                            .window
                            .as_ref()
                            .context("missing native window")?
                            .inner_size();
                        let pane = PaneRender {
                            id: app.workspace.active_pane,
                            rect: [0.0, 0.0, size.width as f32, size.height as f32],
                            screen: &screen,
                            selection: None,
                            focused: true,
                            preedit: None,
                        };
                        renderer.render_workspace(
                            &[pane],
                            &[],
                            &egui::TexturesDelta::default(),
                            app.window
                                .as_ref()
                                .map_or(1.0, |window| window.scale_factor() as f32),
                            terminal_render::GlassEffect::Balanced,
                        )?;
                        let balanced = renderer.diagnostics();
                        let material_enabled = renderer.native_glass_enabled();
                        renderer.render(&screen)?;
                        let off_after = renderer.diagnostics();
                        let opaque = [&off_before, &off_after].iter().all(|surface| {
                            surface.contains("alpha=Opaque")
                                && surface.contains("window_opaque=true")
                                && surface.contains("metal_layer_opaque=Some(true)")
                        });
                        self.native_mode_observations = vec![
                            serde_json::json!({"effect":"Off", "surface":off_before}),
                            serde_json::json!({"effect":"Balanced", "surface":balanced}),
                            serde_json::json!({"effect":"Off", "surface":off_after}),
                        ];
                        self.check(
                            "native Off mode uses an opaque window and Metal surface",
                            opaque,
                        )?;
                        self.check(
                            "native glass can be restored after Off",
                            terminal_render::accessibility_preferences().reduce_transparency
                                || material_enabled,
                        )?;
                    }
                    renderer.screenshot(&screen, self.dir.join("terminal.png"))?;
                }
                app.presented(frames_before);
                std::fs::write(
                    self.dir.join("screen.json"),
                    serde_json::to_vec_pretty(&app.active().screen)?,
                )?;
                self.idle_frames = app.frames;
                app.ui.settings.effect = config::Effect::Balanced;
                app.redraw();
                self.next();
            }
            7 if self.phase_started.elapsed() > Duration::from_millis(500)
                && app.frames > self.idle_frames =>
            {
                self.begin_idle(app);
            }
            8 if self.phase_started.elapsed() > Duration::from_secs(3) => {
                let idle = self.observe_idle(app, "Balanced");
                self.check("idle window stops redrawing", idle)?;
                self.idle_frames = app.frames;
                app.ui.settings.effect = config::Effect::Full;
                app.redraw();
                self.next();
            }
            9 if self.phase_started.elapsed() > Duration::from_millis(500)
                && app.frames > self.idle_frames =>
            {
                self.begin_idle(app);
            }
            10 if self.phase_started.elapsed() > Duration::from_secs(3) => {
                let idle = self.observe_idle(app, "Full");
                self.check("Full glass idle window stops redrawing", idle)?;
                app.active_mut()
                    .session
                    .as_mut()
                    .context("missing session")?
                    .input(b"exit 7\r")?;
                self.next();
            }
            11 if self.exit_code.is_some() => {
                self.check("shell exit status", self.exit_code == Some(7))?;
                app.close_session();
                self.check(
                    "native GPU screenshot saved",
                    self.dir.join("terminal.png").is_file(),
                )?;
                self.collect_gpu_timing(app);
                self.write_result(app, true)?;
                event_loop.exit();
                self.next();
            }
            6 if self.phase_started.elapsed() > Duration::from_secs(2) => {
                self.write_result(app, false)?;
                anyhow::bail!("foreground process failed to interrupt within two seconds");
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn write_result(&self, app: &App, passed: bool) -> Result<()> {
        let result = serde_json::json!({
            "schema":"glass-terminal.native-spike", "passed":passed,
            "complete":passed && self.unverified_checks.is_empty(),
            "phase":self.phase,
            "unverified_checks":self.unverified_checks,
            "resize_request_applied":self.resize_observed,
            "platform": std::env::consts::OS, "arch":std::env::consts::ARCH,
            "native_window":true,"native_pty":self.ready,"synthetic_input":true,
            "physical_keyboard":false,"system_ime_candidates":false,
            "helper_pid":self.helper_pid,"shell_pid":self.shell_pid,
            "shell_exit_code":self.exit_code,"checks":self.checks,
            "rendered_frames":app.frames,"elapsed_ms":self.started.elapsed().as_millis(),
            "startup_to_first_present_ms":app.first_present_ms,
            "gpu_adapter":app.renderer.as_ref().map(|renderer| format!("{:?}",renderer.adapter_info())),
            "renderer_diagnostics":app.renderer.as_ref().map(Renderer::diagnostics),
            "final_terminal_text":app.active().screen.plain_text().chars().take(16_384).collect::<String>(),
            "final_cols":app.active().screen.cols,"final_rows":app.active().screen.rows,
            "idle_observation":self.idle_observation,
            "idle_observations":self.idle_observations,
            "native_mode_observations":self.native_mode_observations,
            "gpu_timing":self.gpu_timing,
        });
        std::fs::write(
            self.dir.join("final-screen.json"),
            serde_json::to_vec_pretty(&app.active().screen)?,
        )?;
        std::fs::write(
            self.dir.join("RESULT.json"),
            serde_json::to_vec_pretty(&result)?,
        )?;
        Ok(())
    }
}

// FVPROV1 18140 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiYTU3OTQ5MmVjYTg1MjEwYzg4MjUxZjdkNWQ0MDEyZjdiYTBmMTUxNGQ5YTNiZTFlYzFkNGQ3ZDE2NmUwZjdjMSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiODE2YTJmZDFlNDdiOTQ3MmY3YWQwYWNkMjQ1NTJkZTkxYWE1MjhiNDExY2JmOTEzZGY4ODY0ODZhM2ZkNDYxODg3ZDFhOWNkZTNlNmE0M2I1MGVjOTIwOTMxN2JkOGM5MWUyMWVmYzg5ZmQzYWVmOWYwZWRiZGIyZjc5ZGFlMGEifV19XQ== FVPROV1-END

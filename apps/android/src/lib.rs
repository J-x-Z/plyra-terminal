#![cfg(target_os = "android")]

mod arch_import;

use glass_terminal_desktop::android_input::{self, AndroidInput, PlatformAction, PointerPhase};
use glass_terminal_desktop::{Options, Wake};
use jni::{
    jni_sig, jni_str,
    objects::{JByteArray, JClass},
    sys::{jboolean, jdouble, jint, jlong, JNI_FALSE, JNI_TRUE},
    EnvUnowned, JValue,
};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::fd::FromRawFd,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};
use winit::event_loop::{EventLoop, EventLoopProxy};
use winit::platform::android::activity::AndroidApp;
use winit::platform::android::EventLoopBuilderExtAndroid;

const MAX_INPUT_BYTES: usize = 16 * 1024;
static INPUT_PROXY: Mutex<Option<EventLoopProxy<Wake>>> = Mutex::new(None);
static STARTUP_READY: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

fn send_input(input: AndroidInput) -> bool {
    let proxy = INPUT_PROXY.lock().ok().and_then(|proxy| proxy.clone());
    proxy.is_some_and(|proxy| android_input::send_input(&proxy, input))
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn Java_org_plyra_glass_terminal_GlassTerminalActivity_nativeImportArch<
    'local,
>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    fd: jint,
    offset: jlong,
    length: jlong,
    bundled: jboolean,
    dns: JByteArray<'local>,
) {
    let bundled = bundled == JNI_TRUE;
    if fd < 0 {
        let _ = send_input(AndroidInput::ArchImportFinished {
            bundled,
            result: Err("The Arch archive descriptor is invalid".into()),
        });
        return;
    }
    // SAF or the APK asset supplies ownership via ParcelFileDescriptor.detachFd().
    let mut archive = unsafe { File::from_raw_fd(fd) };
    let addresses = unowned_env
        .with_env(|env| -> jni::errors::Result<Vec<std::net::IpAddr>> {
            if dns.len(env)? > 256 {
                return Ok(Vec::new());
            }
            let bytes = env.convert_byte_array(&dns)?;
            Ok(String::from_utf8_lossy(&bytes)
                .lines()
                .filter_map(|line| line.parse().ok())
                .take(3)
                .collect())
        })
        .resolve::<jni::errors::LogErrorAndDefault>();
    let result = (|| -> anyhow::Result<()> {
        anyhow::ensure!(offset >= 0 && length >= -1, "Invalid Arch archive bounds");
        if offset > 0 {
            archive.seek(SeekFrom::Start(offset as u64))?;
        }
        let limit = if length < 0 { u64::MAX } else { length as u64 };
        let files_dir = system_api::android_root::files_dir()?;
        arch_import::install(archive.take(limit), &files_dir, &addresses)
    })()
    .map_err(|error| format!("{error:#}"));
    let _ = send_input(AndroidInput::ArchImportFinished { bundled, result });
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn Java_org_plyra_glass_terminal_GlassTerminalActivity_nativeArchOpenFailed<
    'local,
>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    bundled: jboolean,
    message: JByteArray<'local>,
) {
    let error = unowned_env
        .with_env(|env| -> jni::errors::Result<String> {
            if message.len(env)? > 2048 {
                return Ok("Cannot open Arch archive".into());
            }
            Ok(String::from_utf8_lossy(&env.convert_byte_array(&message)?).into_owned())
        })
        .resolve::<jni::errors::LogErrorAndDefault>();
    let _ = send_input(AndroidInput::ArchImportFinished {
        bundled: bundled == JNI_TRUE,
        result: Err(if error.is_empty() {
            "Cannot open Arch archive".into()
        } else {
            error
        }),
    });
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn Java_org_plyra_glass_terminal_GlassTerminalActivity_nativeGlassState(
    _env: EnvUnowned<'_>,
    _class: JClass<'_>,
    api: jint,
    available: jboolean,
    active: jboolean,
    power_save: jboolean,
    thermal_status: jint,
    radius: jint,
    reason: jint,
) -> jboolean {
    if terminal_render::android_glass::update_state(
        api,
        available == JNI_TRUE,
        active == JNI_TRUE,
        power_save == JNI_TRUE,
        thermal_status,
        radius,
        reason,
    ) {
        if let Some(proxy) = INPUT_PROXY.lock().ok().and_then(|proxy| proxy.clone()) {
            let _ = proxy.send_event(Wake::Repaint(Duration::ZERO));
        }
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn Java_org_plyra_glass_terminal_GlassTerminalActivity_nativeInputDiagnostics(
    _env: EnvUnowned<'_>,
    _class: JClass<'_>,
    enabled: jboolean,
) {
    android_input::set_input_diagnostics(cfg!(debug_assertions) && enabled == JNI_TRUE);
}

#[no_mangle]
pub fn android_main(app: AndroidApp) {
    let configured = STARTUP_READY.0.lock().expect("Android startup state lock");
    let (configured, _) = STARTUP_READY
        .1
        .wait_timeout_while(configured, Duration::from_secs(5), |ready| !*ready)
        .expect("Android startup configuration wait");
    assert!(
        *configured,
        "Android Activity did not provide its private application paths"
    );
    drop(configured);
    let event_loop = EventLoop::<Wake>::with_user_event()
        .with_android_app(app)
        .build()
        .expect("create Android event loop");

    *INPUT_PROXY.lock().expect("Android input proxy lock") = Some(event_loop.create_proxy());
    let result = glass_terminal_desktop::run(event_loop, Options::default());
    *INPUT_PROXY.lock().expect("Android input proxy lock") = None;
    if let Err(error) = result {
        panic!("Plyra Terminal Android startup failed: {error:#}");
    }
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn Java_org_plyra_glass_terminal_GlassTerminalActivity_nativeInput<'local>(
    mut unowned_env: EnvUnowned<'local>,
    _class: JClass<'local>,
    kind: jint,
    bytes: JByteArray<'local>,
    modifiers: jint,
) -> jboolean {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jboolean> {
            let length = bytes.len(env)?;
            if length > MAX_INPUT_BYTES || !(0..=7).contains(&modifiers) {
                return Ok(JNI_FALSE);
            }
            let Ok(text) = String::from_utf8(env.convert_byte_array(&bytes)?) else {
                return Ok(JNI_FALSE);
            };
            let event = match kind {
                0 => AndroidInput::Commit(text),
                1 => AndroidInput::Preedit(text),
                2 if text.len() <= 64 => AndroidInput::Key {
                    name: text,
                    modifiers: modifiers as u8,
                },
                3 if text.is_empty() => AndroidInput::ImeEnabled,
                4 if text.is_empty() => AndroidInput::ImeDisabled,
                5 => AndroidInput::Paste(text),
                6 if text.is_empty() => AndroidInput::Copy,
                7 if text.is_empty() => AndroidInput::Cut,
                8 if text.is_empty() => AndroidInput::SelectAll,
                _ => return Ok(JNI_FALSE),
            };
            let sent = send_input(event);
            Ok(if sent { JNI_TRUE } else { JNI_FALSE })
        })
        .resolve::<jni::errors::LogErrorAndDefault>()
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn Java_org_plyra_glass_terminal_GlassTerminalActivity_nativeConfigure<
    'local,
>(
    mut unowned_env: EnvUnowned<'local>,
    class: JClass<'local>,
    files: JByteArray<'local>,
    libraries: JByteArray<'local>,
    bundled_arch: jboolean,
    android_api_name: JByteArray<'local>,
) -> jboolean {
    unowned_env
        .with_env(|env| -> jni::errors::Result<jboolean> {
            if files.len(env)? > 4096
                || libraries.len(env)? > 4096
                || android_api_name.len(env)? > 128
            {
                return Ok(JNI_FALSE);
            }
            let Ok(files) = String::from_utf8(env.convert_byte_array(&files)?) else {
                return Ok(JNI_FALSE);
            };
            let Ok(libraries) = String::from_utf8(env.convert_byte_array(&libraries)?) else {
                return Ok(JNI_FALSE);
            };
            let Ok(android_api_name) =
                String::from_utf8(env.convert_byte_array(&android_api_name)?)
            else {
                return Ok(JNI_FALSE);
            };
            let context = system_api::android_root::AndroidContext {
                files_dir: PathBuf::from(files),
                native_library_dir: PathBuf::from(libraries),
            };
            if let Err(error) = system_api::android_root::configure(context) {
                eprintln!("Android private application paths: {error:#}");
                return Ok(JNI_FALSE);
            }
            android_input::configure_arch_bootstrap(bundled_arch == JNI_TRUE, android_api_name);
            let class = Arc::new(env.new_global_ref(&class)?);
            let vm = Arc::new(env.get_java_vm()?);
            let glass_class = class.clone();
            let glass_vm = vm.clone();
            if !terminal_render::android_glass::install_handler(Arc::new(move |enabled, radius| {
                let result: jni::errors::Result<()> = glass_vm.attach_current_thread(|env| {
                    env.call_static_method(
                        &**glass_class,
                        jni_str!("configureGlass"),
                        jni_sig!("(ZF)V"),
                        &[JValue::Bool(enabled), JValue::Float(radius)],
                    )?;
                    Ok(())
                });
                result.map_err(|error| error.to_string())
            })) {
                return Ok(JNI_FALSE);
            }
            android_input::install_platform_handler(Arc::new(move |action| {
                let result: jni::errors::Result<()> = vm.attach_current_thread(|env| {
                    match action {
                        PlatformAction::CopyText(text) => {
                            let text = env.new_string(text)?;
                            env.call_static_method(
                                &**class,
                                jni_str!("copyText"),
                                jni_sig!("(Ljava/lang/String;)V"),
                                &[JValue::Object(text.as_ref())],
                            )?;
                        }
                        PlatformAction::RequestPaste => {
                            env.call_static_method(
                                &**class,
                                jni_str!("requestPaste"),
                                jni_sig!("()V"),
                                &[],
                            )?;
                        }
                        PlatformAction::PickArchArchive => {
                            env.call_static_method(
                                &**class,
                                jni_str!("pickArchArchive"),
                                jni_sig!("()V"),
                                &[],
                            )?;
                        }
                        PlatformAction::PrepareBundledArch => {
                            env.call_static_method(
                                &**class,
                                jni_str!("prepareBundledArch"),
                                jni_sig!("()V"),
                                &[],
                            )?;
                        }
                        PlatformAction::Keyboard(visible) => {
                            env.call_static_method(
                                &**class,
                                jni_str!("requestKeyboard"),
                                jni_sig!("(Z)V"),
                                &[JValue::Bool(visible)],
                            )?;
                        }
                        PlatformAction::OpenUrl(url) => {
                            let url = env.new_string(url)?;
                            env.call_static_method(
                                &**class,
                                jni_str!("openUrl"),
                                jni_sig!("(Ljava/lang/String;)V"),
                                &[JValue::Object(url.as_ref())],
                            )?;
                        }
                        PlatformAction::OpenFile { name, contents } => {
                            let name = env.new_string(name)?;
                            let contents = env.byte_array_from_slice(&contents)?;
                            env.call_static_method(
                                &**class,
                                jni_str!("openManagedFile"),
                                jni_sig!("(Ljava/lang/String;[B)V"),
                                &[
                                    JValue::Object(name.as_ref()),
                                    JValue::Object(contents.as_ref()),
                                ],
                            )?;
                        }
                    }
                    Ok(())
                });
                result.map_err(|error| error.to_string())
            }));
            let Ok(mut ready) = STARTUP_READY.0.lock() else {
                return Ok(JNI_FALSE);
            };
            *ready = true;
            STARTUP_READY.1.notify_all();
            Ok(JNI_TRUE)
        })
        .resolve::<jni::errors::LogErrorAndDefault>()
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn Java_org_plyra_glass_terminal_GlassTerminalActivity_nativeViewport(
    _env: EnvUnowned<'_>,
    _class: JClass<'_>,
    x: jint,
    y: jint,
    width: jint,
    height: jint,
) -> jboolean {
    if x < 0
        || y < 0
        || width <= 0
        || height <= 0
        || x.saturating_add(width) > 65_536
        || y.saturating_add(height) > 65_536
    {
        return JNI_FALSE;
    }
    let sent = send_input(AndroidInput::Viewport {
        x: x as u32,
        y: y as u32,
        width: width as u32,
        height: height as u32,
    });
    if sent {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn Java_org_plyra_glass_terminal_GlassTerminalActivity_nativePointer(
    _env: EnvUnowned<'_>,
    _class: JClass<'_>,
    phase: jint,
    x: jdouble,
    y: jdouble,
) -> jboolean {
    if ![x, y]
        .iter()
        .all(|value| value.is_finite() && value.abs() <= 1_000_000.0)
    {
        return JNI_FALSE;
    }
    let phase = match phase {
        0 => PointerPhase::Started,
        1 => PointerPhase::Moved,
        2 => PointerPhase::Ended,
        3 => PointerPhase::Cancelled,
        _ => return JNI_FALSE,
    };
    if send_input(AndroidInput::Pointer { phase, x, y }) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn Java_org_plyra_glass_terminal_GlassTerminalActivity_nativeScroll(
    _env: EnvUnowned<'_>,
    _class: JClass<'_>,
    x: jdouble,
    y: jdouble,
    dx: jdouble,
    dy: jdouble,
) -> jboolean {
    if ![x, y, dx, dy]
        .iter()
        .all(|value| value.is_finite() && value.abs() <= 1_000_000.0)
    {
        return JNI_FALSE;
    }
    if send_input(AndroidInput::Scroll { x, y, dx, dy }) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

// FVPROV1 15402 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiMDJkZDE4ZjYzNzFlMDQwN2I3ZmEzNTI4MjgxMjIzZGUyZmEzMzYwYWUzOGU3ODQzNzgxZTg2M2FlOTdmMDhjMyIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMTdlMmEyNWU1MTRhNDAxYzI2ZDEwNWJhYzk5NTdkMGZmZmFhZDIyNThmOTFiNDY2YTVhYzBhNjdjODgzMjgwOGEyMGI5ZTUwMjA4Njk0OWI5MzJlMmU3ZGMyYWY4OTUxM2IwNWQ0OWI5MjAxZjg5ZTYzNTlhMzJhZTk1NmE5MGUifV19XQ== FVPROV1-END

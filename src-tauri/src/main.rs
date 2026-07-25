// Sin consola en las compilaciones de release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod groq;
mod paste;
#[cfg(windows)]
mod win;

use config::Config;
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, State, WebviewWindow};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

const ISLAND: &str = "island";
const SETTINGS: &str = "settings";
const STATE_EVENT: &str = "riff://state";
const LEVEL_EVENT: &str = "riff://level";
const PREVIEW_EVENT: &str = "riff://preview";

/// Espera máxima a que se vacíe la cola de frases al insertar.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(45);

/// Fases del dictado. El atajo principal alterna entre grabar y pausar tantas veces como
/// haga falta; `Enter` inserta el texto y `Esc` lo descarta.
const IDLE: u8 = 0;
const RECORDING: u8 = 1;
const PAUSED: u8 = 2;

struct AppState {
    /// `mpsc::Sender` es `Send` pero no `Sync`, y el estado gestionado por Tauri exige
    /// ambos. El Mutex es lo que lo hace compartible entre hilos.
    audio: Mutex<Sender<audio::Cmd>>,
    /// Frases listas para transcribir, en orden de llegada. No se lee desde aquí: se
    /// conserva para mantener vivo el canal mientras la aplicación exista.
    #[allow(dead_code)]
    segments: Mutex<Sender<(usize, Vec<u8>)>>,
    phase: AtomicU8,
    /// Frases encoladas o en vuelo. Sirve para saber cuándo hemos terminado del todo.
    pending: Arc<AtomicUsize>,
    /// Identificador de la sesión de dictado. Al cancelar se incrementa, y las frases
    /// que aún estuvieran en vuelo se descartan al no coincidir.
    session: Arc<AtomicUsize>,
    /// Frases ya pulidas de la sesión. Se muestran en la isla mientras hablas y se pegan
    /// enteras al pulsar Enter, nunca por partes.
    ///
    /// Es una lista y no un texto porque cada frase aparece primero en bruto (recién salida
    /// de Whisper) y se sustituye por su versión pulida cuando llega: guardarlas por
    /// separado permite reemplazar solo la última sin tocar lo anterior.
    transcript: Mutex<Vec<String>>,
    clipboard: Mutex<Option<String>>,
    config: Mutex<Config>,
}

impl AppState {
    fn config(&self) -> Config {
        self.config.lock().expect("config envenenada").clone()
    }

    fn send_audio(&self, cmd: audio::Cmd) {
        if let Ok(audio) = self.audio.lock() {
            let _ = audio.send(cmd);
        }
    }
}

// ---------------------------------------------------------------- interfaz

fn island(app: &AppHandle) -> Option<WebviewWindow> {
    app.get_webview_window(ISLAND)
}

fn emit_state(app: &AppHandle, state: &str, message: Option<&str>) {
    let _ = app.emit(
        STATE_EVENT,
        serde_json::json!({ "state": state, "message": message }),
    );
}

/// Centrada horizontalmente y algo por encima de la barra de tareas.
fn position_island(window: &WebviewWindow) {
    let Ok(Some(monitor)) = window.primary_monitor() else {
        return;
    };
    let Ok(size) = window.outer_size() else {
        return;
    };
    let screen = monitor.size();
    let x = (screen.width as i32 - size.width as i32) / 2;
    let y = screen.height as i32 - size.height as i32 - 70;
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

fn show_island(app: &AppHandle) {
    if let Some(window) = island(app) {
        position_island(&window);
        let _ = window.show();
    }
}

fn hide_island(app: &AppHandle) {
    if let Some(window) = island(app) {
        let _ = window.hide();
    }
}

/// Oculta la isla tras una pausa, salvo que el usuario ya haya empezado otro dictado.
fn hide_island_after(app: &AppHandle, millis: u64) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(millis));
        let still_idle = app
            .try_state::<AppState>()
            .map(|state| state.phase.load(Ordering::SeqCst) == IDLE)
            .unwrap_or(true);
        if still_idle {
            hide_island(&app);
        }
    });
}

fn open_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(SETTINGS) {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

// ---------------------------------------------------------------- dictado

/// El atajo principal: arranca, pausa y reanuda, tantas veces como se pulse.
fn toggle(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    match state.phase.load(Ordering::SeqCst) {
        RECORDING => pause_recording(app),
        PAUSED => resume_recording(app),
        _ => start_recording(app),
    }
}

fn pause_recording(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    if state
        .phase
        .compare_exchange(RECORDING, PAUSED, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    state.send_audio(audio::Cmd::Pause);
    emit_state(app, "paused", None);
}

fn resume_recording(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    if state
        .phase
        .compare_exchange(PAUSED, RECORDING, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    state.send_audio(audio::Cmd::Resume);
    emit_state(app, "listening", None);
}

fn start_recording(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };

    if state.config().api_key.trim().is_empty() {
        open_settings(app);
        return;
    }

    // Si no estaba en reposo, otra pulsación se nos adelantó.
    if state
        .phase
        .compare_exchange(IDLE, RECORDING, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    // Nueva sesión: lo que quedase en vuelo de la anterior deja de ser válido.
    state.session.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut transcript) = state.transcript.lock() {
        transcript.clear();
    }
    if let Ok(mut clipboard) = state.clipboard.lock() {
        *clipboard = paste::snapshot();
    }
    let _ = app.emit(PREVIEW_EVENT, "");

    state.send_audio(audio::Cmd::Start);
    show_island(app);
    emit_state(app, "listening", None);

    grab_session_keys(app);
}

/// La isla nunca tiene el foco, así que `Enter` y `Esc` hay que capturarlos a nivel de
/// sistema. Solo se toman mientras dura el dictado: mientras tanto ninguna otra aplicación
/// los recibe, y por eso se sueltan en cuanto se inserta o se cancela.
fn grab_session_keys(app: &AppHandle) {
    let global = app.global_shortcut();
    if let Ok(escape) = Shortcut::from_str("Escape") {
        let _ = global.register(escape);
    }
    if let Ok(enter) = Shortcut::from_str("Enter") {
        let _ = global.register(enter);
    }
}

fn release_session_keys(app: &AppHandle) {
    let global = app.global_shortcut();
    if let Ok(escape) = Shortcut::from_str("Escape") {
        let _ = global.unregister(escape);
    }
    if let Ok(enter) = Shortcut::from_str("Enter") {
        let _ = global.unregister(enter);
    }
}

fn cancel_recording(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    if state.phase.swap(IDLE, Ordering::SeqCst) == IDLE {
        return;
    }
    release_session_keys(app);

    // Invalida las frases que sigan en vuelo: se descartarán sin llegar a la isla.
    state.session.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut transcript) = state.transcript.lock() {
        transcript.clear();
    }

    let (tx, rx) = mpsc::channel();
    state.send_audio(audio::Cmd::Stop(tx));
    std::thread::spawn(move || {
        let _ = rx.recv();
    });

    restore_clipboard(app);
    hide_island(app);
}

fn restore_clipboard(app: &AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        if let Ok(mut clipboard) = state.clipboard.lock() {
            if let Some(previous) = clipboard.take() {
                paste::restore(previous);
            }
        }
    }
}

/// `Enter`: cierra el dictado, espera a las frases que queden en la cola e inserta todo.
fn finish(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    if state.phase.swap(IDLE, Ordering::SeqCst) == IDLE {
        return;
    }
    release_session_keys(app);

    let (tx, rx) = mpsc::channel();
    state.send_audio(audio::Cmd::Stop(tx));

    let pending = state.pending.clone();
    let handle = app.clone();

    std::thread::spawn(move || {
        // El hilo de audio responde cuando ha encolado la última frase pendiente.
        let audio_result = rx.recv();

        if let Ok(Err(error)) = audio_result {
            emit_state(&handle, "error", Some(&error));
            hide_island_after(&handle, 2600);
            restore_clipboard(&handle);
            return;
        }

        if pending.load(Ordering::SeqCst) > 0 {
            emit_state(&handle, "transcribing", None);
        }

        let deadline = Instant::now() + DRAIN_TIMEOUT;
        while pending.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(80));
        }

        // Ahora sí: todo el dictado se entrega de una sola vez.
        let text = handle
            .try_state::<AppState>()
            .and_then(|state| state.transcript.lock().ok().map(|t| t.join(" ")))
            .unwrap_or_default()
            .trim()
            .to_string();

        if text.is_empty() {
            emit_state(&handle, "error", Some("No se entendió nada"));
            hide_island_after(&handle, 2000);
            restore_clipboard(&handle);
            return;
        }

        match paste::paste_text(&text) {
            Ok(()) => {
                emit_state(&handle, "done", None);
                hide_island_after(&handle, 650);
            }
            Err(error) => {
                // El texto sigue en el portapapeles: el dictado no se ha perdido.
                emit_state(
                    &handle,
                    "error",
                    Some(&format!("{error}. Está copiado: pega con Ctrl+V")),
                );
                hide_island_after(&handle, 3200);
            }
        }

        restore_clipboard(&handle);
    });
}

/// Transcribe una frase, la pule y la añade a la vista previa de la isla.
///
/// No pega nada: el texto se entrega entero al soltar el atajo. Así el usuario ve que se
/// le está entendiendo, pero su documento no se toca hasta que él termina de hablar.
/// Se ejecutan de una en una y en orden.
async fn process_segment(app: &AppHandle, session_id: usize, wav: Vec<u8>) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };

    // La sesión cambió (el usuario canceló o empezó otro dictado): esto ya no vale.
    if state.session.load(Ordering::SeqCst) != session_id {
        return;
    }

    let config = state.config();

    let transcript = match groq::transcribe(&config.api_key, wav, &config.language).await {
        Ok(text) => text,
        Err(error) => {
            emit_state(app, "error", Some(&error));
            hide_island_after(app, 2600);
            return;
        }
    };

    if transcript.trim().is_empty() {
        return;
    }

    // Se enseña ya la versión en bruto de Whisper, sin esperar al pulido. Es la mayor
    // ganancia de latencia percibida: el texto aparece medio segundo antes y luego se
    // sustituye por la versión con la puntuación corregida. Como la vista previa no toca
    // el documento del usuario, no hay ningún riesgo en mostrar un borrador.
    let text = if config.polish {
        if let Ok(confirmed) = state.transcript.lock() {
            let _ = app.emit(PREVIEW_EVENT, preview_with_draft(&confirmed, &transcript));
        }
        groq::polish(&config.api_key, &transcript).await
    } else {
        transcript
    };

    if text.trim().is_empty() {
        return;
    }

    // Se revisa otra vez: el pulido puede haber tardado y la sesión pudo cancelarse.
    if state.session.load(Ordering::SeqCst) != session_id {
        return;
    }

    let full = {
        let Ok(mut transcript) = state.transcript.lock() else {
            return;
        };
        transcript.push(text.trim().to_string());
        transcript.join(" ")
    };

    let _ = app.emit(PREVIEW_EVENT, full);
}

/// Une lo ya confirmado con la frase que todavía se está puliendo.
fn preview_with_draft(confirmed: &[String], draft: &str) -> String {
    let mut parts: Vec<&str> = confirmed.iter().map(|s| s.as_str()).collect();
    parts.push(draft);
    parts.join(" ")
}

// ---------------------------------------------------------------- bandeja

fn apply_shortcut(app: &AppHandle, shortcut: &str) -> bool {
    let global = app.global_shortcut();
    // Se sueltan todos y se vuelve a poner solo el principal; Escape se registra al grabar.
    let _ = global.unregister_all();

    match Shortcut::from_str(shortcut) {
        Ok(parsed) => global.register(parsed).is_ok(),
        Err(_) => false,
    }
}

fn shortcut_option(
    app: &AppHandle,
    choice: &str,
    config: &Config,
) -> tauri::Result<CheckMenuItem<tauri::Wry>> {
    CheckMenuItem::with_id(
        app,
        format!("shortcut:{choice}"),
        choice,
        true,
        config.shortcut == choice,
        None::<&str>,
    )
}

fn build_tray(app: &AppHandle, config: &Config) -> tauri::Result<()> {
    let heading = MenuItem::with_id(
        app,
        "heading",
        format!("Riff · {}", config.shortcut),
        false,
        None::<&str>,
    )?;

    // Explícitos en vez de un bucle: construir un `Vec<&dyn IsMenuItem<_>>` obliga a
    // nombrar el runtime genérico y no aporta nada con solo cuatro opciones.
    let alt_r = shortcut_option(app, "Alt+R", config)?;
    let alt_j = shortcut_option(app, "Alt+J", config)?;
    let alt_q = shortcut_option(app, "Alt+Q", config)?;
    let ctrl_space = shortcut_option(app, "Ctrl+Space", config)?;
    let shortcut_menu = Submenu::with_id_and_items(
        app,
        "shortcuts",
        "Atajo",
        true,
        &[&alt_r, &alt_j, &alt_q, &ctrl_space],
    )?;

    let polish = CheckMenuItem::with_id(
        app,
        "polish",
        "Pulir con IA",
        true,
        config.polish,
        None::<&str>,
    )?;
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "Iniciar con Windows",
        true,
        app.autolaunch().is_enabled().unwrap_or(false),
        None::<&str>,
    )?;
    let settings = MenuItem::with_id(app, "settings", "Ajustes…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Salir de Riff", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &heading,
            &PredefinedMenuItem::separator(app)?,
            &shortcut_menu,
            &polish,
            &autostart,
            &PredefinedMenuItem::separator(app)?,
            &settings,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id("tray")
        .tooltip("Riff · dictado por voz")
        .menu(&menu)
        .on_menu_event(|app, event| handle_menu(app, event.id().as_ref()));

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;

    Ok(())
}

fn handle_menu(app: &AppHandle, id: &str) {
    match id {
        "quit" => app.exit(0),
        "settings" => open_settings(app),
        "polish" => {
            if let Some(state) = app.try_state::<AppState>() {
                let mut config = state.config.lock().expect("config envenenada");
                config.polish = !config.polish;
                let _ = config::save(app, &config);
            }
        }
        "autostart" => {
            let launcher = app.autolaunch();
            if launcher.is_enabled().unwrap_or(false) {
                let _ = launcher.disable();
            } else {
                let _ = launcher.enable();
            }
        }
        other => {
            let Some(choice) = other.strip_prefix("shortcut:") else {
                return;
            };
            if !apply_shortcut(app, choice) {
                // Si otra aplicación ya lo tenía, se vuelve al anterior.
                if let Some(state) = app.try_state::<AppState>() {
                    let previous = state.config().shortcut;
                    apply_shortcut(app, &previous);
                }
                return;
            }
            if let Some(state) = app.try_state::<AppState>() {
                let mut config = state.config.lock().expect("config envenenada");
                config.shortcut = choice.to_string();
                let _ = config::save(app, &config);
            }
        }
    }
}

// ---------------------------------------------------------------- comandos

#[tauri::command]
fn get_config(state: State<AppState>) -> Config {
    state.config()
}

#[tauri::command]
fn save_api_key(app: AppHandle, state: State<AppState>, api_key: String) -> Result<(), String> {
    let mut config = state.config.lock().map_err(|_| "config envenenada")?;
    config.api_key = api_key.trim().to_string();
    config::save(&app, &config)
}

#[tauri::command]
fn close_settings(app: AppHandle) {
    if let Some(window) = app.get_webview_window(SETTINGS) {
        let _ = window.hide();
    }
}

// ---------------------------------------------------------------- arranque

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let is_escape = shortcut.matches(Modifiers::empty(), Code::Escape);
                    let is_enter = shortcut.matches(Modifiers::empty(), Code::Enter);
                    let app = app.clone();

                    // El trabajo se hace en otro hilo y este callback retorna al instante.
                    //
                    // Es obligatorio: mientras el plugin despacha el evento mantiene tomado
                    // su cerrojo interno, así que registrar o soltar atajos aquí dentro
                    // (cosa que hace start_recording con Escape) bloquea la aplicación
                    // entera. Además, un hook global nunca debe entretenerse.
                    std::thread::spawn(move || {
                        if is_escape {
                            cancel_recording(&app);
                        } else if is_enter {
                            finish(&app);
                        } else {
                            toggle(&app);
                        }
                    });
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_api_key,
            close_settings
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let config = config::load(&handle);

            let pending = Arc::new(AtomicUsize::new(0));
            let session = Arc::new(AtomicUsize::new(0));

            // El hilo de audio informa del nivel; la isla lo dibuja.
            let level_handle = handle.clone();
            let on_level = Arc::new(move |level: f32| {
                let _ = level_handle.emit(LEVEL_EVENT, level);
            });

            // Cada frase cerrada entra en la cola con el identificador de su sesión.
            let (segment_tx, segment_rx) = mpsc::channel::<(usize, Vec<u8>)>();
            let queue = segment_tx.clone();
            let queue_pending = pending.clone();
            let queue_session = session.clone();
            let on_segment = Arc::new(move |wav: Vec<u8>| {
                queue_pending.fetch_add(1, Ordering::SeqCst);
                let _ = queue.send((queue_session.load(Ordering::SeqCst), wav));
            });

            // Un único consumidor: así las frases se pegan en el mismo orden en que se
            // dijeron, aunque una tarde más que otra en transcribirse.
            let worker = handle.clone();
            let worker_pending = pending.clone();
            std::thread::spawn(move || {
                while let Ok((session_id, wav)) = segment_rx.recv() {
                    tauri::async_runtime::block_on(process_segment(&worker, session_id, wav));
                    worker_pending.fetch_sub(1, Ordering::SeqCst);
                }
            });

            app.manage(AppState {
                audio: Mutex::new(audio::spawn(on_level, on_segment)),
                segments: Mutex::new(segment_tx),
                phase: AtomicU8::new(IDLE),
                pending,
                session,
                transcript: Mutex::new(Vec::new()),
                clipboard: Mutex::new(None),
                config: Mutex::new(config.clone()),
            });

            if let Some(window) = island(&handle) {
                // Sin esto la isla robaría el foco y el pegado no tendría destino.
                #[cfg(windows)]
                if let Ok(hwnd) = window.hwnd() {
                    win::make_non_activating(hwnd.0 as isize);
                }
                position_island(&window);
            }

            build_tray(&handle, &config)?;

            if !apply_shortcut(&handle, &config.shortcut) {
                eprintln!(
                    "[riff] no se pudo registrar {}: otra aplicación lo tiene tomado",
                    config.shortcut
                );
            }

            if config.api_key.trim().is_empty() {
                open_settings(&handle);
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // Cerrar Ajustes no debe terminar la aplicación: Riff vive en la bandeja.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == SETTINGS {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("Riff no pudo arrancar");
}

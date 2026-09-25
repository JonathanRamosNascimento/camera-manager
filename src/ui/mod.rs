//! Montagem da janela e ligação entre as pipelines GStreamer e os widgets GTK.
//!
//! Divisão de threads:
//! - **thread do GTK**: cria as pipelines (o `GdkPaintable` do sink precisa
//!   nascer aqui), monta o grid, aplica mudanças de estado, captura PNG e
//!   dispara notificações;
//! - **runtime do tokio**: um `Supervisor` por câmera (bus, watchdog,
//!   health-check, backoff, gravação) e o serviço da bandeja.
//!
//! Três canais ligam as duas metades, todos consumidos por
//! `glib::spawn_future_local` ou por tasks do tokio — nada de rede ou de I/O
//! bloqueia o main loop:
//!
//! ```text
//!   supervisores ── CameraEvent ──▶ UI
//!   UI ─────────── Command ───────▶ supervisores
//!   widgets ────── UiAction ──────▶ UI (evita ciclos Rc entre tile e janela)
//!   bandeja ────── TrayCommand ───▶ UI        UI ── TraySummary ──▶ bandeja
//! ```

pub mod camera_tile;
pub mod fullscreen;
pub mod grid;
pub mod quality;
pub mod snapshot;

use std::cell::RefCell;
use std::convert::Infallible;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use crate::camera::{Camera, Quality, Redactor};
use crate::config::Config;
use crate::notify::{Notifier, TrayCommand, TraySummary};
use crate::pipeline::{self, PipelineOptions, StreamStats};
use crate::reconnect::{
    Backoff, CameraEvent, CameraState, Command, EventKind, RecordingStatus, Supervisor,
};
use crate::recording::RecordingOptions;
use camera_tile::CameraTile;
use fullscreen::FullscreenView;

const APP_ID: &str = "io.github.nvrdashboard.NvrDashboard";

/// Por quanto tempo o selo de movimento fica aceso na view de tela cheia.
const MOTION_BADGE_DURATION: Duration = Duration::from_secs(6);

/// Quanto a janela espera pelos supervisores antes de fechar à força.
/// Precisa ser maior que o `recording::STOP_TIMEOUT`.
const CLOSE_GRACE_SECS: u32 = 8;

/// Ações originadas nos widgets e nos atalhos de teclado.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAction {
    /// Abrir a câmera em tela cheia.
    Open(usize),
    /// Voltar ao grid.
    Back,
    Snapshot(usize),
    ToggleRecording(usize),
    /// Qualidade da imagem no grid, escolhida no seletor do tile.
    SetQuality(usize, Quality),
}

/// Tudo que o callback `activate` precisa; `Rc` porque o callback é `Fn`.
struct Bootstrap {
    config: Config,
    cameras: Vec<Camera>,
    tokio: tokio::runtime::Handle,
}

/// Sobe a aplicação GTK e bloqueia até a janela fechar.
/// Fecha quando todos os supervisores terminaram de encerrar.
pub type SupervisorsDone = async_channel::Receiver<Infallible>;

/// O que `build_window` entrega de volta para `run`.
type WindowHandles = (SupervisorsDone, Vec<gst::Pipeline>);

/// O que a UI devolve ao `main` quando a janela fecha.
pub struct Session {
    pub exit_code: glib::ExitCode,
    /// Fecha quando o último supervisor sai — o `main` espera nele para não
    /// cortar a finalização de um arquivo de gravação.
    pub supervisors_done: SupervisorsDone,
    /// Âncora de tempo de vida, nunca usada.
    ///
    /// As pipelines contêm o `gtk4paintablesink`, que guarda objetos afins à
    /// thread do GTK. Se o último `Drop` da pipeline cair numa thread do tokio
    /// — o que acontece quando um supervisor termina depois da janela fechar —
    /// o glib aborta o processo com "Value dropped on a different thread than
    /// where it was created". Mantendo uma referência viva aqui até o
    /// `process::exit`, que não roda destrutores, esse `Drop` nunca acontece.
    pub _pipelines: Vec<gst::Pipeline>,
}

pub fn run(config: Config, cameras: Vec<Camera>, tokio: tokio::runtime::Handle) -> Result<Session> {
    let bootstrap = Rc::new(Bootstrap {
        config,
        cameras,
        tokio,
    });
    // `build_window` devolve por aqui o que o `main` precisa: esses valores só
    // existem depois que as pipelines e os supervisores nascem.
    let slot: Rc<RefCell<Option<WindowHandles>>> = Rc::new(RefCell::new(None));

    let app = gtk::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        // Dashboard de monitoramento vive em tela escura; sem isso a barra de
        // título seguiria o tema claro do sistema e destoaria dos tiles.
        if let Some(settings) = gtk::Settings::default() {
            settings.set_gtk_application_prefer_dark_theme(true);
        }
        camera_tile::load_css();
    });
    let activate_slot = Rc::clone(&slot);
    app.connect_activate(move |app| {
        *activate_slot.borrow_mut() = Some(build_window(app, &bootstrap));
    });

    app.set_accels_for_action("win.close", &["<Ctrl>q", "<Ctrl>w"]);
    app.set_accels_for_action("win.fullscreen", &["F11"]);
    app.set_accels_for_action("win.back", &["Escape"]);
    // 1–9 abrem a câmera daquela posição, como em qualquer software de NVR.
    for position in 1..=9u8 {
        let key = position.to_string();
        app.set_accels_for_action(&format!("win.open({position})"), &[&key]);
    }
    app.set_accels_for_action("win.snapshot", &["<Ctrl>s"]);
    app.set_accels_for_action("win.record", &["<Ctrl>r"]);

    // A CLI já consumiu os argumentos; não deixamos o GTK reinterpretá-los.
    let exit_code = app.run_with_args::<&str>(&[]);
    let (supervisors_done, pipelines) = slot.borrow_mut().take().unwrap_or_else(|| {
        // A janela nunca abriu: devolve um canal já fechado, para o `main` não
        // ficar esperando supervisores que não existem.
        let (sender, receiver) = async_channel::bounded::<Infallible>(1);
        drop(sender);
        (receiver, Vec::new())
    });
    Ok(Session {
        exit_code,
        supervisors_done,
        _pipelines: pipelines,
    })
}

// ---------------------------------------------------------------------------
// Estado da janela
// ---------------------------------------------------------------------------

/// Estado vivo do dashboard, compartilhado pelos loops assíncronos da UI.
struct Dashboard {
    window: gtk::ApplicationWindow,
    stack: gtk::Stack,
    summary: gtk::Label,
    fullscreen: FullscreenView,
    notifier: Notifier,

    cameras: Vec<Camera>,
    /// Qualidade escolhida por câmera para o grid (índice = id da câmera).
    grid_quality: RefCell<Vec<Quality>>,
    tiles: Vec<CameraTile>,
    paintables: Vec<Option<gdk::Paintable>>,
    commands: Vec<async_channel::Sender<Command>>,

    states: RefCell<Vec<CameraState>>,
    /// O usuário pediu gravação nesta câmera?
    recording_wanted: RefCell<Vec<bool>>,
    /// A gravação está de fato acontecendo?
    recording_active: RefCell<Vec<bool>>,

    snapshot_dir: PathBuf,
    adaptive_stream: bool,
    notify_motion: bool,
    tray_updates: async_channel::Sender<TraySummary>,
    last_tray_summary: RefCell<TraySummary>,
}

const PAGE_GRID: &str = "grid";
const PAGE_SINGLE: &str = "single";

impl Dashboard {
    // -- navegação ----------------------------------------------------------

    /// Abre a câmera em tela cheia e, com `adaptive_stream`, pede o stream
    /// principal ao supervisor.
    fn open(&self, id: usize) {
        let Some(camera) = self.cameras.get(id) else {
            return;
        };
        self.fullscreen.show(camera, self.paintables[id].as_ref());
        self.fullscreen.set_state(&self.states.borrow()[id]);
        self.fullscreen
            .set_recording(self.recording_active.borrow()[id]);
        self.stack.set_visible_child_name(PAGE_SINGLE);
        if self.adaptive_stream {
            self.send(id, Command::UseStream(camera.main_stream));
        }
        tracing::debug!(camera = %camera.label(), "tela cheia aberta");
    }

    /// Aplica a qualidade escolhida no seletor: troca o stream do grid e guarda.
    fn set_quality(&self, id: usize, quality: Quality) {
        let Some(camera) = self.cameras.get(id) else {
            return;
        };
        if self.grid_quality.borrow()[id] == quality {
            return;
        }
        self.grid_quality.borrow_mut()[id] = quality;
        self.send(id, Command::UseStream(camera.stream_for(quality)));

        let prefs = self
            .cameras
            .iter()
            .zip(self.grid_quality.borrow().iter())
            .map(|(camera, quality)| (camera.pref_key(), *quality))
            .collect();
        quality::save(&prefs);
        tracing::info!(camera = %camera.label(), ?quality, "qualidade alterada");
    }

    /// Volta ao grid e devolve a câmera à qualidade escolhida para o grid.
    fn back(&self) {
        let Some(id) = self.fullscreen.current() else {
            return;
        };
        self.stack.set_visible_child_name(PAGE_GRID);
        self.fullscreen.clear();
        if let Some(camera) = self.cameras.get(id) {
            let quality = self.grid_quality.borrow()[id];
            self.send(id, Command::UseStream(camera.stream_for(quality)));
        }
        self.tiles[id].widget().grab_focus();
    }

    /// Câmera alvo dos atalhos: a de tela cheia ou a que está com o foco.
    ///
    /// O foco pode estar num botão dentro do tile, então subimos a árvore de
    /// widgets em vez de exigir foco no tile em si.
    fn focused_camera(&self) -> Option<usize> {
        if let Some(id) = self.fullscreen.current() {
            return Some(id);
        }
        if let Some(focus) = gtk::prelude::RootExt::focus(&self.window)
            && let Some(id) = self.tiles.iter().position(|tile| {
                let root = tile.widget();
                focus == *root || focus.is_ancestor(root)
            })
        {
            return Some(id);
        }
        // Com uma câmera só não há ambiguidade.
        (self.tiles.len() == 1).then_some(0)
    }

    // -- ações --------------------------------------------------------------

    fn snapshot(&self, id: usize) {
        let (Some(camera), Some(Some(paintable))) = (self.cameras.get(id), self.paintables.get(id))
        else {
            return;
        };
        match snapshot::capture(&self.window, paintable, &self.snapshot_dir, &camera.slug()) {
            Ok(path) => {
                tracing::info!(camera = %camera.label(), arquivo = %path.display(), "captura salva");
                self.toast(&format!("Captura salva em {}", path.display()));
            }
            Err(err) => {
                let reason = format!("{err:#}");
                tracing::warn!(camera = %camera.label(), erro = %reason, "falha na captura");
                self.toast(&format!("Não consegui capturar: {reason}"));
            }
        }
    }

    fn toggle_recording(&self, id: usize) {
        let Some(camera) = self.cameras.get(id) else {
            return;
        };
        let wanted = {
            let mut flags = self.recording_wanted.borrow_mut();
            flags[id] = !flags[id];
            flags[id]
        };
        self.send(
            id,
            if wanted {
                Command::StartRecording
            } else {
                Command::StopRecording
            },
        );
        tracing::info!(camera = %camera.label(), gravando = wanted, "gravação alternada");
        self.refresh_recording(id);
    }

    fn send(&self, id: usize, command: Command) {
        if let Some(sender) = self.commands.get(id)
            && sender.try_send(command.clone()).is_err()
        {
            tracing::warn!(camera = id, ?command, "supervisor não recebeu o comando");
        }
    }

    /// Mensagem curta na barra de título — feedback sem abrir diálogo.
    fn toast(&self, text: &str) {
        self.summary.set_label(text);
        self.summary.set_tooltip_text(Some(text));
    }

    // -- eventos ------------------------------------------------------------

    fn apply_event(&self, event: CameraEvent) {
        let id = event.camera_id;
        let (Some(tile), Some(camera)) = (self.tiles.get(id), self.cameras.get(id)) else {
            tracing::warn!(id, "evento de câmera desconhecida");
            return;
        };

        match event.kind {
            EventKind::State(state) => {
                tile.set_state(&state);
                if self.fullscreen.current() == Some(id) {
                    self.fullscreen.set_state(&state);
                }
                match &state {
                    CameraState::Live => self.notifier.camera_recovered(id, &camera.name),
                    CameraState::Reconnecting {
                        attempt, reason, ..
                    } => self
                        .notifier
                        .camera_offline(id, &camera.name, *attempt, reason),
                    CameraState::Failed(reason) => {
                        self.notifier
                            .camera_offline(id, &camera.name, u32::MAX, reason)
                    }
                    // Nem "conectando" nem "aguardando keyframe" são falha:
                    // não geram notificação.
                    CameraState::Connecting | CameraState::WaitingKeyframe => {}
                }
                self.states.borrow_mut()[id] = state;
            }
            EventKind::Recording(status) => {
                match status {
                    RecordingStatus::Started { pattern } => {
                        self.recording_active.borrow_mut()[id] = true;
                        self.toast(&format!("Gravando {}", camera.name));
                        tracing::debug!(camera = %camera.label(), arquivos = %pattern, "gravando");
                    }
                    RecordingStatus::Stopped => {
                        self.recording_active.borrow_mut()[id] = false;
                    }
                    RecordingStatus::Failed(reason) => {
                        self.recording_active.borrow_mut()[id] = false;
                        self.recording_wanted.borrow_mut()[id] = false;
                        self.notifier.recording_failed(&camera.name, &reason);
                        self.toast(&format!("Falha na gravação: {reason}"));
                    }
                }
                self.refresh_recording(id);
            }
            EventKind::Motion => {
                if self.notify_motion {
                    self.notifier.motion(&camera.name);
                }
            }
        }
        self.update_summary();
    }

    fn refresh_recording(&self, id: usize) {
        let active = self.recording_active.borrow()[id];
        let wanted = self.recording_wanted.borrow()[id];
        self.tiles[id].set_recording(active, wanted);
        if self.fullscreen.current() == Some(id) {
            self.fullscreen.set_recording(active);
        }
    }

    // -- atualização periódica ----------------------------------------------

    fn tick(&self) {
        let focused = self.fullscreen.current();
        for (id, tile) in self.tiles.iter().enumerate() {
            let detail = tile.sample();
            tile.tick();
            if focused == Some(id) {
                self.fullscreen.set_detail(&detail);
                self.fullscreen
                    .set_motion(tile.stats().motion_recent(MOTION_BADGE_DURATION));
            }
        }
        self.update_summary();
        self.publish_tray();
    }

    fn update_summary(&self) {
        let live = self.tiles.iter().filter(|tile| tile.is_live()).count();
        let recording = self.tiles.iter().filter(|tile| tile.is_recording()).count();
        let mut text = format!("{live}/{} ao vivo", self.tiles.len());
        if recording > 0 {
            text.push_str(&format!(" · {recording} gravando"));
        }
        self.summary.set_label(&text);
        self.summary.set_tooltip_text(None);
    }

    fn publish_tray(&self) {
        let summary = TraySummary {
            total: self.tiles.len(),
            live: self.tiles.iter().filter(|tile| tile.is_live()).count(),
            recording: self.tiles.iter().filter(|tile| tile.is_recording()).count(),
            offline: self
                .tiles
                .iter()
                .enumerate()
                .filter(|(_, tile)| !tile.is_live())
                .map(|(id, _)| self.cameras[id].name.clone())
                .collect(),
        };
        // Só falamos com a bandeja quando algo muda de verdade.
        if *self.last_tray_summary.borrow() == summary {
            return;
        }
        *self.last_tray_summary.borrow_mut() = summary.clone();
        let _ = self.tray_updates.try_send(summary);
    }
}

// ---------------------------------------------------------------------------
// Construção da janela
// ---------------------------------------------------------------------------

/// Monta a janela e devolve o que o `main` precisa segurar: o canal de
/// conclusão dos supervisores e uma referência às pipelines.
fn build_window(app: &gtk::Application, bootstrap: &Bootstrap) -> WindowHandles {
    let config = &bootstrap.config;
    let hosts: Vec<&str> = config
        .all_nvrs()
        .iter()
        .map(|nvr| nvr.host.as_str())
        .collect();

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(format!("NVR Dashboard — {}", hosts.join(", ")))
        .default_width(1280)
        .default_height(760)
        .css_classes(["nvr-dashboard"])
        .build();

    let summary = gtk::Label::builder()
        .label("—")
        .css_classes(["app-summary"])
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .max_width_chars(48)
        .build();
    let header = gtk::HeaderBar::new();
    header.pack_end(&summary);
    window.set_titlebar(Some(&header));

    // supervisores → UI, widgets → UI, bandeja ↔ UI
    let (events_tx, events_rx) = async_channel::bounded::<CameraEvent>(64);
    let (actions_tx, actions_rx) = async_channel::bounded::<UiAction>(32);
    let (tray_tx, tray_rx) = async_channel::bounded::<TraySummary>(8);
    let (tray_cmd_tx, tray_cmd_rx) = async_channel::bounded::<TrayCommand>(8);
    // fechado no `close-request`; acorda todos os supervisores de uma vez
    let (shutdown_tx, shutdown_rx) = async_channel::bounded::<Infallible>(1);
    // `done_tx` é local: quando esta função retorna, só os supervisores seguram
    // clones dele, então o canal fecha exatamente quando o último sai.
    let (done_tx, done_rx) = async_channel::bounded::<Infallible>(1);

    let options = PipelineOptions::from_config(config);
    let backoff = Backoff::new(
        config.app.reconnect_initial_secs,
        config.app.reconnect_max_secs,
    );
    let stall_timeout = Duration::from_secs(config.app.stall_timeout_secs);
    // Sem espera por keyframe não há vão longo a tolerar: os dois prazos
    // colapsam num só e o watchdog volta a ser o mais rígido.
    let keyframe_timeout = if config.app.wait_for_keyframe {
        Duration::from_secs(config.app.keyframe_timeout_secs)
    } else {
        stall_timeout
    };
    let redactor = Redactor::new(config);
    let recording_options = RecordingOptions::from_config(config);

    // Preferências salvas de qualidade valem sobre o padrão do config.
    let prefs = quality::load();
    let cameras: Vec<Camera> = bootstrap
        .cameras
        .iter()
        .cloned()
        .map(|mut camera| {
            if camera.has_substream()
                && let Some(&quality) = prefs.get(&camera.pref_key())
            {
                camera.grid_stream = camera.stream_for(quality);
            }
            camera
        })
        .collect();

    let count = cameras.len();
    let mut tiles = Vec::with_capacity(count);
    let mut paintables = Vec::with_capacity(count);
    let mut commands = Vec::with_capacity(count);
    let mut pipelines = Vec::with_capacity(count);

    for camera in &cameras {
        let (command_tx, command_rx) = async_channel::bounded::<Command>(8);
        commands.push(command_tx);

        match pipeline::build(camera, &options) {
            Ok(built) => {
                let stats = Arc::clone(&built.handle.stats);
                tiles.push(CameraTile::new(
                    camera,
                    Some(&built.paintable),
                    stats,
                    &actions_tx,
                ));
                paintables.push(Some(built.paintable));
                pipelines.push(built.handle.pipeline.clone());

                bootstrap.tokio.spawn(
                    Supervisor {
                        camera: camera.clone(),
                        handle: built.handle,
                        backoff,
                        stall_timeout,
                        keyframe_timeout,
                        redactor: redactor.clone(),
                        recording_options: recording_options.clone(),
                        events: events_tx.clone(),
                        commands: command_rx,
                        shutdown: shutdown_rx.clone(),
                        _done_guard: done_tx.clone(),
                    }
                    .run(),
                );
            }
            // Uma pipeline que não sobe não pode derrubar as outras: o tile
            // entra em `Failed` e o resto do grid segue normalmente.
            Err(err) => {
                let reason = format!("{err:#}");
                tracing::error!(
                    camera = %camera.label(),
                    erro = %reason,
                    "não consegui construir a pipeline"
                );
                let tile =
                    CameraTile::new(camera, None, Arc::new(StreamStats::default()), &actions_tx);
                tile.set_state(&CameraState::Failed(reason));
                tiles.push(tile);
                paintables.push(None);
            }
        }
    }

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(120)
        .build();
    stack.add_named(
        &grid::build(&tiles, config.app.grid_columns),
        Some(PAGE_GRID),
    );
    let fullscreen = FullscreenView::new(&actions_tx);
    stack.add_named(fullscreen.widget(), Some(PAGE_SINGLE));
    stack.set_visible_child_name(PAGE_GRID);
    window.set_child(Some(&stack));

    if config.notifications.tray {
        crate::notify::spawn_tray(&bootstrap.tokio, tray_rx, tray_cmd_tx);
    }

    let dashboard = Rc::new(Dashboard {
        window: window.clone(),
        stack,
        summary,
        fullscreen,
        notifier: Notifier::new(app, &config.notifications),
        grid_quality: RefCell::new(cameras.iter().map(Camera::grid_quality).collect()),
        cameras,
        tiles,
        paintables,
        commands,
        states: RefCell::new(vec![CameraState::Connecting; count]),
        recording_wanted: RefCell::new(vec![false; count]),
        recording_active: RefCell::new(vec![false; count]),
        snapshot_dir: config.snapshot_dir(),
        adaptive_stream: config.app.adaptive_stream,
        notify_motion: config.motion.enabled && config.motion.notify,
        tray_updates: tray_tx,
        last_tray_summary: RefCell::new(TraySummary::default()),
    });

    spawn_loops(&dashboard, events_rx, actions_rx, tray_cmd_rx);
    install_actions(&window, &dashboard);

    install_close_handler(&window, &dashboard, shutdown_tx, done_rx.clone());

    dashboard.update_summary();
    window.present();
    (done_rx, pipelines)
}

/// Liga os três consumidores de canal e o tick de 1 s ao main loop do GLib.
fn spawn_loops(
    dashboard: &Rc<Dashboard>,
    events: async_channel::Receiver<CameraEvent>,
    actions: async_channel::Receiver<UiAction>,
    tray_commands: async_channel::Receiver<TrayCommand>,
) {
    let target = Rc::clone(dashboard);
    glib::spawn_future_local(async move {
        while let Ok(event) = events.recv().await {
            target.apply_event(event);
        }
        tracing::debug!("canal de eventos fechado");
    });

    let target = Rc::clone(dashboard);
    glib::spawn_future_local(async move {
        while let Ok(action) = actions.recv().await {
            match action {
                UiAction::Open(id) => target.open(id),
                UiAction::Back => target.back(),
                UiAction::Snapshot(id) => target.snapshot(id),
                UiAction::ToggleRecording(id) => target.toggle_recording(id),
                UiAction::SetQuality(id, quality) => target.set_quality(id, quality),
            }
        }
    });

    let target = Rc::clone(dashboard);
    glib::spawn_future_local(async move {
        while let Ok(command) = tray_commands.recv().await {
            match command {
                TrayCommand::Present => target.window.present(),
                TrayCommand::Quit => target.window.close(),
            }
        }
    });

    // Lê os contadores atômicos das pipelines: fps, bitrate, movimento.
    let target = Rc::clone(dashboard);
    glib::timeout_add_seconds_local(1, move || {
        target.tick();
        glib::ControlFlow::Continue
    });
}

/// Fecha a janela só depois que os supervisores terminarem.
///
/// Dois motivos para não sair na hora:
/// - parar as pipelines daqui atropelaria o `EOS` que cada supervisor injeta
///   para fechar o arquivo de gravação, e o vídeo sairia truncado;
/// - o `gtk4paintablesink` guarda objetos afins à thread do GTK. Derrubar a
///   pipeline com o main loop já encerrado faz o glib abortar o processo
///   ("Value accessed from different thread than where it was created").
///
/// Então sinalizamos, seguramos a janela, e destruímos quando o canal de
/// conclusão fechar — ou quando o prazo estourar, para não travar a saída.
fn install_close_handler(
    window: &gtk::ApplicationWindow,
    dashboard: &Rc<Dashboard>,
    shutdown: async_channel::Sender<Infallible>,
    done: SupervisorsDone,
) {
    let closing = Rc::new(std::cell::Cell::new(false));
    let dashboard = Rc::clone(dashboard);

    window.connect_close_request(move |window| {
        if closing.replace(true) {
            return glib::Propagation::Proceed;
        }
        tracing::info!("encerrando: sinalizando supervisores");
        shutdown.close();
        dashboard.toast("Finalizando gravações…");

        let window = window.clone();
        let done = done.clone();
        glib::spawn_future_local(async move {
            let finished = std::pin::pin!(done.recv());
            let deadline = std::pin::pin!(glib::timeout_future_seconds(CLOSE_GRACE_SECS));
            let timed_out = matches!(
                futures_util::future::select(finished, deadline).await,
                futures_util::future::Either::Right(_)
            );
            if timed_out {
                tracing::warn!(
                    segundos = CLOSE_GRACE_SECS,
                    "supervisores não encerraram a tempo; fechando assim mesmo"
                );
            }
            window.destroy();
        });

        glib::Propagation::Stop
    });
}

fn install_actions(window: &gtk::ApplicationWindow, dashboard: &Rc<Dashboard>) {
    let close = gio::ActionEntry::builder("close")
        .activate(|window: &gtk::ApplicationWindow, _, _| window.close())
        .build();
    let fullscreen = gio::ActionEntry::builder("fullscreen")
        .activate(|window: &gtk::ApplicationWindow, _, _| {
            if window.is_fullscreen() {
                window.unfullscreen();
            } else {
                window.fullscreen();
            }
        })
        .build();

    let target = Rc::clone(dashboard);
    let open = gio::ActionEntry::builder("open")
        .parameter_type(Some(glib::VariantTy::INT32))
        .activate(move |_: &gtk::ApplicationWindow, _, parameter| {
            // O parâmetro é a posição na tela (1 = primeira câmera).
            if let Some(position) = parameter.and_then(glib::Variant::get::<i32>)
                && position >= 1
            {
                target.open(position as usize - 1);
            }
        })
        .build();

    let target = Rc::clone(dashboard);
    let back = gio::ActionEntry::builder("back")
        .activate(move |_: &gtk::ApplicationWindow, _, _| target.back())
        .build();

    let target = Rc::clone(dashboard);
    let snapshot = gio::ActionEntry::builder("snapshot")
        .activate(
            move |_: &gtk::ApplicationWindow, _, _| match target.focused_camera() {
                Some(id) => target.snapshot(id),
                None => target.toast("Selecione uma câmera para capturar"),
            },
        )
        .build();

    let target = Rc::clone(dashboard);
    let record = gio::ActionEntry::builder("record")
        .activate(
            move |_: &gtk::ApplicationWindow, _, _| match target.focused_camera() {
                Some(id) => target.toggle_recording(id),
                None => target.toast("Selecione uma câmera para gravar"),
            },
        )
        .build();

    window.add_action_entries([close, fullscreen, open, back, snapshot, record]);
}

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
pub mod manage;
pub mod quality;
pub mod snapshot;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use crate::camera::{self, Camera, Quality, Redactor, UrlTemplate};
use crate::config::Config;
use crate::notify::{Notifier, TrayCommand, TraySummary};
use crate::pipeline::{self, PipelineOptions, StreamStats};
use crate::reconnect::{
    self,
    Backoff, CameraEvent, CameraState, Command, EventKind, RecordingStatus, Supervisor,
};
use crate::recording::RecordingOptions;
use crate::store::{Device, Store};
use camera_tile::CameraTile;
use fullscreen::FullscreenView;
use grid::GridView;

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
    /// Abrir a lista de câmeras cadastradas.
    ShowCameras,
    /// Abrir o formulário de cadastro manual.
    AddManual,
    /// Abrir a varredura de rede.
    ScanNetwork,
}

/// Tudo que o callback `activate` precisa; `Rc` porque o callback é `Fn`.
struct Bootstrap {
    config: Rc<Config>,
    /// Entregue ao `Dashboard` na primeira ativação.
    store: RefCell<Option<Store>>,
    tokio: tokio::runtime::Handle,
    /// Toda pipeline já criada, viva até o `process::exit` (ver [`Session`]).
    pipelines: Rc<RefCell<Vec<gst::Pipeline>>>,
}

/// Sobe a aplicação GTK e bloqueia até a janela fechar.
/// Fecha quando todos os supervisores terminaram de encerrar.
pub type SupervisorsDone = async_channel::Receiver<Infallible>;

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
    /// — o que acontece quando um supervisor termina depois da janela fechar
    /// ou da câmera ser removida — o glib aborta o processo com "Value dropped
    /// on a different thread than where it was created". Mantendo uma
    /// referência viva aqui até o `process::exit`, que não roda destrutores,
    /// esse `Drop` nunca acontece.
    pub _pipelines: Vec<gst::Pipeline>,
}

pub fn run(config: Config, store: Store, tokio: tokio::runtime::Handle) -> Result<Session> {
    let bootstrap = Rc::new(Bootstrap {
        config: Rc::new(config),
        store: RefCell::new(Some(store)),
        tokio,
        pipelines: Rc::new(RefCell::new(Vec::new())),
    });
    // `build_window` devolve por aqui o canal de conclusão dos supervisores.
    let slot: Rc<RefCell<Option<SupervisorsDone>>> = Rc::new(RefCell::new(None));

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
    let activate_bootstrap = Rc::clone(&bootstrap);
    app.connect_activate(move |app| {
        // Segunda ativação (o usuário abriu o app de novo): só traz a janela.
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        *activate_slot.borrow_mut() = Some(build_window(app, &activate_bootstrap));
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
    let supervisors_done = slot.borrow_mut().take().unwrap_or_else(|| {
        // A janela nunca abriu: devolve um canal já fechado, para o `main` não
        // ficar esperando supervisores que não existem.
        let (sender, receiver) = async_channel::bounded::<Infallible>(1);
        drop(sender);
        receiver
    });
    let pipelines = std::mem::take(&mut *bootstrap.pipelines.borrow_mut());
    Ok(Session {
        exit_code,
        supervisors_done,
        _pipelines: pipelines,
    })
}

// ---------------------------------------------------------------------------
// Estado da janela
// ---------------------------------------------------------------------------

/// Tudo de uma câmera em exibição.
struct Slot {
    camera: Camera,
    tile: CameraTile,
    paintable: Option<gdk::Paintable>,
    /// `None` depois que a câmera é removida (ou se a pipeline nem subiu).
    /// Largar o remetente é o que faz o supervisor encerrar.
    command: RefCell<Option<async_channel::Sender<Command>>>,
    state: RefCell<CameraState>,
    /// O usuário pediu gravação nesta câmera?
    recording_wanted: Cell<bool>,
    /// A gravação está de fato acontecendo?
    recording_active: Cell<bool>,
    /// Qualidade escolhida para o grid.
    quality: Cell<Quality>,
}

/// O que é preciso para criar pipelines e supervisores depois da janela aberta.
struct Spawner {
    tokio: tokio::runtime::Handle,
    options: PipelineOptions,
    backoff: Backoff,
    stall_timeout: Duration,
    keyframe_timeout: Duration,
    redactor: RefCell<Redactor>,
    recording_options: RecordingOptions,
    events: async_channel::Sender<CameraEvent>,
    shutdown: reconnect::ShutdownSignal,
    /// Cada supervisor segura um clone; vira `None` ao fechar a janela, para o
    /// canal de conclusão poder fechar.
    done: RefCell<Option<async_channel::Sender<Infallible>>>,
    actions: async_channel::Sender<UiAction>,
}

/// Estado vivo do dashboard, compartilhado pelos loops assíncronos da UI.
struct Dashboard {
    window: gtk::ApplicationWindow,
    stack: gtk::Stack,
    summary: gtk::Label,
    fullscreen: FullscreenView,
    notifier: Notifier,
    grid: GridView,
    config: Rc<Config>,
    spawner: Spawner,
    /// Toda pipeline criada, viva até o fim do processo.
    pipelines: Rc<RefCell<Vec<gst::Pipeline>>>,

    store: RefCell<Store>,
    /// Índice = id da câmera. Ids nunca são reaproveitados: uma câmera removida
    /// deixa `None`, assim eventos atrasados do supervisor não caem noutra.
    slots: RefCell<Vec<Option<Rc<Slot>>>>,
    /// Qualidade escolhida por câmera (`<dispositivo>/<canal>`), persistida.
    quality_prefs: RefCell<BTreeMap<String, Quality>>,
    /// Quem quer saber quando a lista de câmeras muda (janela "Câmeras").
    on_cameras_changed: RefCell<Option<Box<dyn Fn()>>>,

    snapshot_dir: PathBuf,
    adaptive_stream: bool,
    notify_motion: bool,
    tray_updates: async_channel::Sender<TraySummary>,
    last_tray_summary: RefCell<TraySummary>,
}

const PAGE_EMPTY: &str = "empty";
const PAGE_GRID: &str = "grid";
const PAGE_SINGLE: &str = "single";

impl Dashboard {
    fn slot(&self, id: usize) -> Option<Rc<Slot>> {
        self.slots.borrow().get(id).and_then(Clone::clone)
    }

    /// Câmeras atualmente no dashboard, por id crescente.
    fn live_slots(&self) -> Vec<Rc<Slot>> {
        self.slots.borrow().iter().flatten().cloned().collect()
    }

    // -- navegação ----------------------------------------------------------

    /// Abre a câmera em tela cheia e, com `adaptive_stream`, pede o stream
    /// principal ao supervisor.
    fn open(&self, id: usize) {
        let Some(slot) = self.slot(id) else {
            return;
        };
        self.fullscreen.show(&slot.camera, slot.paintable.as_ref());
        self.fullscreen.set_state(&slot.state.borrow());
        self.fullscreen.set_recording(slot.recording_active.get());
        self.stack.set_visible_child_name(PAGE_SINGLE);
        if self.adaptive_stream {
            self.send(id, Command::UseStream(slot.camera.main_stream));
        }
        tracing::debug!(camera = %slot.camera.label(), "tela cheia aberta");
    }

    /// Abre a câmera que está na posição `position` (1 = primeira) da tela.
    fn open_position(&self, position: usize) {
        if let Some(&id) = self.grid.ids().get(position.saturating_sub(1)) {
            self.open(id);
        }
    }

    /// Aplica a qualidade escolhida no seletor: troca o stream do grid e guarda.
    fn set_quality(&self, id: usize, quality: Quality) {
        let Some(slot) = self.slot(id) else {
            return;
        };
        if slot.quality.get() == quality {
            return;
        }
        slot.quality.set(quality);
        self.send(id, Command::UseStream(slot.camera.stream_for(quality)));

        let mut prefs = self.quality_prefs.borrow_mut();
        prefs.insert(slot.camera.pref_key(), quality);
        quality::save(&prefs);
        tracing::info!(camera = %slot.camera.label(), ?quality, "qualidade alterada");
    }

    /// Volta ao grid e devolve a câmera à qualidade escolhida para o grid.
    fn back(&self) {
        let Some(id) = self.fullscreen.current() else {
            return;
        };
        self.fullscreen.clear();
        if let Some(slot) = self.slot(id) {
            self.send(id, Command::UseStream(slot.camera.stream_for(slot.quality.get())));
            slot.tile.widget().grab_focus();
        }
        self.refresh_page();
    }

    /// Mostra o estado vazio quando não há câmeras, e o grid quando há.
    fn refresh_page(&self) {
        if self.fullscreen.current().is_some() {
            return;
        }
        let empty = self.slots.borrow().iter().all(Option::is_none);
        self.stack
            .set_visible_child_name(if empty { PAGE_EMPTY } else { PAGE_GRID });
    }

    /// Câmera alvo dos atalhos: a de tela cheia ou a que está com o foco.
    ///
    /// O foco pode estar num botão dentro do tile, então subimos a árvore de
    /// widgets em vez de exigir foco no tile em si.
    fn focused_camera(&self) -> Option<usize> {
        if let Some(id) = self.fullscreen.current() {
            return Some(id);
        }
        let slots = self.live_slots();
        if let Some(focus) = gtk::prelude::RootExt::focus(&self.window)
            && let Some(slot) = slots.iter().find(|slot| {
                let root = slot.tile.widget();
                focus == *root || focus.is_ancestor(root)
            })
        {
            return Some(slot.camera.id);
        }
        // Com uma câmera só não há ambiguidade.
        match slots.as_slice() {
            [only] => Some(only.camera.id),
            _ => None,
        }
    }

    // -- cadastro de câmeras -------------------------------------------------

    /// Cadastra um dispositivo (ou completa um já existente) e põe as câmeras
    /// novas no ar. Devolve quantas câmeras entraram no dashboard.
    fn add_device(&self, device: Device) -> Result<usize> {
        let previous = self
            .store
            .borrow()
            .devices
            .iter()
            .find(|d| d.id == device.id)
            .cloned();
        let credentials_changed = previous.as_ref().is_some_and(|old| {
            old.username != device.username
                || old.password != device.password
                || old.url_template != device.url_template
        });

        let added = self.store.borrow_mut().add(device.clone())?;
        if let Err(err) = self.store.borrow().save() {
            tracing::error!(erro = %format!("{err:#}"), "não consegui salvar o cadastro");
            self.toast(&format!("Câmeras adicionadas, mas não consegui salvar: {err:#}"));
        }

        let stored = self
            .store
            .borrow()
            .devices
            .iter()
            .find(|d| d.id == device.id)
            .cloned()
            .expect("dispositivo recém-adicionado");
        self.spawner
            .redactor
            .borrow_mut()
            .add_password(stored.password.expose());

        // Login novo: as câmeras já no ar usam o antigo, então recomeçam.
        let channels = if credentials_changed {
            let stale: Vec<usize> = self
                .live_slots()
                .iter()
                .filter(|slot| slot.camera.nvr_id == stored.id)
                .map(|slot| slot.camera.id)
                .collect();
            for id in stale {
                self.detach_camera(id);
            }
            stored.channels.clone()
        } else {
            added
        };

        let urls = Arc::new(UrlTemplate::new(&stored));
        let count = channels.len();
        for entry in &channels {
            let id = self.slots.borrow().len();
            let camera = camera::build_one(id, &stored, &urls, entry, &self.config.app);
            self.spawn_camera(camera);
        }
        self.cameras_changed();
        Ok(count)
    }

    /// Remove a câmera do dashboard **e** do cadastro.
    fn remove_camera(&self, id: usize) {
        let Some(slot) = self.slot(id) else {
            return;
        };
        self.detach_camera(id);
        self.store
            .borrow_mut()
            .remove_channel(&slot.camera.nvr_id, slot.camera.channel);
        if let Err(err) = self.store.borrow().save() {
            tracing::error!(erro = %format!("{err:#}"), "não consegui salvar o cadastro");
            self.toast(&format!("Não consegui salvar o cadastro: {err:#}"));
        }
        tracing::info!(camera = %slot.camera.label(), "câmera removida");
        self.cameras_changed();
    }

    /// Tira a câmera de cena: encerra o supervisor e some com o card.
    fn detach_camera(&self, id: usize) {
        let Some(slot) = self.slot(id) else {
            return;
        };
        if self.fullscreen.current() == Some(id) {
            self.back();
        }
        // O supervisor percebe o canal fechado, finaliza uma eventual gravação
        // e sai.
        slot.command.borrow_mut().take();
        self.grid.remove(id);
        self.slots.borrow_mut()[id] = None;
    }

    fn cameras_changed(&self) {
        self.refresh_page();
        self.update_summary();
        if let Some(callback) = &*self.on_cameras_changed.borrow() {
            callback();
        }
    }

    /// Cria pipeline, card e supervisor de uma câmera.
    fn spawn_camera(&self, mut camera: Camera) {
        let id = camera.id;
        // Preferência salva de qualidade vale sobre o padrão do config.
        let quality = match self.quality_prefs.borrow().get(&camera.pref_key()) {
            Some(&saved) if camera.has_substream() => {
                camera.grid_stream = camera.stream_for(saved);
                saved
            }
            _ => camera.grid_quality(),
        };

        let (command_tx, command_rx) = async_channel::bounded::<Command>(8);
        let mut command = Some(command_tx);
        let (tile, paintable) = match pipeline::build(&camera, &self.spawner.options) {
            Ok(built) => {
                let tile = CameraTile::new(
                    &camera,
                    Some(&built.paintable),
                    Arc::clone(&built.handle.stats),
                    &self.spawner.actions,
                );
                self.pipelines.borrow_mut().push(built.handle.pipeline.clone());

                match self.spawner.done.borrow().clone() {
                    Some(done_guard) => {
                        self.spawner.tokio.spawn(
                            Supervisor {
                                camera: camera.clone(),
                                handle: built.handle,
                                backoff: self.spawner.backoff,
                                stall_timeout: self.spawner.stall_timeout,
                                keyframe_timeout: self.spawner.keyframe_timeout,
                                redactor: self.spawner.redactor.borrow().clone(),
                                recording_options: self.spawner.recording_options.clone(),
                                events: self.spawner.events.clone(),
                                commands: command_rx,
                                shutdown: self.spawner.shutdown.clone(),
                                _done_guard: done_guard,
                            }
                            .run(),
                        );
                    }
                    // Janela fechando: não sobe supervisor novo.
                    None => command = None,
                }
                (tile, Some(built.paintable))
            }
            // Uma pipeline que não sobe não pode derrubar as outras: o card
            // entra em `Failed` e o resto do grid segue normalmente.
            Err(err) => {
                let reason = format!("{err:#}");
                tracing::error!(
                    camera = %camera.label(),
                    erro = %reason,
                    "não consegui construir a pipeline"
                );
                let tile = CameraTile::new(
                    &camera,
                    None,
                    Arc::new(StreamStats::default()),
                    &self.spawner.actions,
                );
                tile.set_state(&CameraState::Failed(reason));
                command = None;
                (tile, None)
            }
        };

        self.grid
            .add(camera.pref_key(), id, tile.widget().clone());
        let mut slots = self.slots.borrow_mut();
        debug_assert_eq!(slots.len(), id, "ids de câmera são sequenciais");
        slots.push(Some(Rc::new(Slot {
            camera,
            tile,
            paintable,
            command: RefCell::new(command),
            state: RefCell::new(CameraState::Connecting),
            recording_wanted: Cell::new(false),
            recording_active: Cell::new(false),
            quality: Cell::new(quality),
        })));
    }

    // -- ações --------------------------------------------------------------

    fn snapshot(&self, id: usize) {
        let Some(slot) = self.slot(id) else {
            return;
        };
        let Some(paintable) = &slot.paintable else {
            return;
        };
        let camera = &slot.camera;
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
        let Some(slot) = self.slot(id) else {
            return;
        };
        let wanted = !slot.recording_wanted.get();
        slot.recording_wanted.set(wanted);
        self.send(
            id,
            if wanted {
                Command::StartRecording
            } else {
                Command::StopRecording
            },
        );
        tracing::info!(camera = %slot.camera.label(), gravando = wanted, "gravação alternada");
        self.refresh_recording(id);
    }

    fn send(&self, id: usize, command: Command) {
        let Some(slot) = self.slot(id) else {
            return;
        };
        if let Some(sender) = &*slot.command.borrow()
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
        let Some(slot) = self.slot(id) else {
            // Normal: eventos que ainda estavam a caminho quando a câmera saiu.
            tracing::debug!(id, "evento de câmera removida");
            return;
        };
        let (tile, camera) = (&slot.tile, &slot.camera);

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
                *slot.state.borrow_mut() = state;
            }
            EventKind::Recording(status) => {
                match status {
                    RecordingStatus::Started { pattern } => {
                        slot.recording_active.set(true);
                        self.toast(&format!("Gravando {}", camera.name));
                        tracing::debug!(camera = %camera.label(), arquivos = %pattern, "gravando");
                    }
                    RecordingStatus::Stopped => slot.recording_active.set(false),
                    RecordingStatus::Failed(reason) => {
                        slot.recording_active.set(false);
                        slot.recording_wanted.set(false);
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
        let Some(slot) = self.slot(id) else {
            return;
        };
        let (active, wanted) = (slot.recording_active.get(), slot.recording_wanted.get());
        slot.tile.set_recording(active, wanted);
        if self.fullscreen.current() == Some(id) {
            self.fullscreen.set_recording(active);
        }
    }

    // -- atualização periódica ----------------------------------------------

    fn tick(&self) {
        let focused = self.fullscreen.current();
        for slot in self.live_slots() {
            let detail = slot.tile.sample();
            slot.tile.tick();
            if focused == Some(slot.camera.id) {
                self.fullscreen.set_detail(&detail);
                self.fullscreen
                    .set_motion(slot.tile.stats().motion_recent(MOTION_BADGE_DURATION));
            }
        }
        self.update_summary();
        self.publish_tray();
    }

    fn update_summary(&self) {
        let slots = self.live_slots();
        if slots.is_empty() {
            self.summary.set_label("Nenhuma câmera");
            self.summary.set_tooltip_text(None);
            return;
        }
        let live = slots.iter().filter(|s| s.tile.is_live()).count();
        let recording = slots.iter().filter(|s| s.tile.is_recording()).count();
        let mut text = format!("{live}/{} ao vivo", slots.len());
        if recording > 0 {
            text.push_str(&format!(" · {recording} gravando"));
        }
        self.summary.set_label(&text);
        self.summary.set_tooltip_text(None);
    }

    fn publish_tray(&self) {
        let slots = self.live_slots();
        let summary = TraySummary {
            total: slots.len(),
            live: slots.iter().filter(|s| s.tile.is_live()).count(),
            recording: slots.iter().filter(|s| s.tile.is_recording()).count(),
            offline: slots
                .iter()
                .filter(|s| !s.tile.is_live())
                .map(|s| s.camera.name.clone())
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

/// Página exibida quando não há nenhuma câmera cadastrada.
fn empty_page(actions: &async_channel::Sender<UiAction>) -> gtk::Widget {
    let button = |label: &str, action: UiAction, suggested: bool| {
        let button = gtk::Button::with_label(label);
        if suggested {
            button.add_css_class("suggested-action");
        }
        button.add_css_class("pill");
        let sender = actions.clone();
        button.connect_clicked(move |_| {
            let _ = sender.try_send(action);
        });
        button
    };

    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["empty-state"])
        .build();
    page.append(
        &gtk::Image::builder()
            .icon_name("camera-video-symbolic")
            .pixel_size(72)
            .css_classes(["empty-icon"])
            .build(),
    );
    page.append(
        &gtk::Label::builder()
            .label("Nenhuma câmera ainda")
            .css_classes(["empty-title"])
            .build(),
    );
    page.append(
        &gtk::Label::builder()
            .label("Escaneie a rede para achar suas câmeras automaticamente\nou adicione uma manualmente.")
            .justify(gtk::Justification::Center)
            .css_classes(["empty-subtitle"])
            .build(),
    );
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .build();
    buttons.append(&button("Escanear a rede", UiAction::ScanNetwork, true));
    buttons.append(&button("Adicionar manualmente", UiAction::AddManual, false));
    buttons.append(&button("Abrir lista de câmeras", UiAction::ShowCameras, false));
    page.append(&buttons);
    page.upcast()
}

/// Monta a janela e devolve o canal de conclusão dos supervisores.
///
/// Nasce sem câmeras; as do cadastro entram uma a uma pelo mesmo caminho das
/// adicionadas depois pela interface.
fn build_window(app: &gtk::Application, bootstrap: &Bootstrap) -> SupervisorsDone {
    let config = Rc::clone(&bootstrap.config);
    let store = bootstrap
        .store
        .borrow_mut()
        .take()
        .expect("build_window roda uma vez");

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("NVR Dashboard")
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

    // supervisores → UI, widgets → UI, bandeja ↔ UI
    let (events_tx, events_rx) = async_channel::bounded::<CameraEvent>(64);
    let (actions_tx, actions_rx) = async_channel::bounded::<UiAction>(32);
    let (tray_tx, tray_rx) = async_channel::bounded::<TraySummary>(8);
    let (tray_cmd_tx, tray_cmd_rx) = async_channel::bounded::<TrayCommand>(8);
    // fechado no `close-request`; acorda todos os supervisores de uma vez
    let (shutdown_tx, shutdown_rx) = async_channel::bounded::<Infallible>(1);
    // Os supervisores seguram clones de `done_tx`; o canal fecha quando o
    // último sai (e o `Dashboard` larga o seu no `close-request`).
    let (done_tx, done_rx) = async_channel::bounded::<Infallible>(1);

    let header = gtk::HeaderBar::new();
    // Ícone + texto: um `Button` com `label` e `icon_name` mostraria só o ícone.
    let cameras_content = gtk::Box::builder().spacing(6).build();
    cameras_content.append(&gtk::Image::from_icon_name("camera-video-symbolic"));
    cameras_content.append(&gtk::Label::new(Some("Câmeras")));
    let cameras_button = gtk::Button::builder()
        .child(&cameras_content)
        .tooltip_text("Lista de câmeras: adicionar, escanear a rede ou remover")
        .build();
    {
        let sender = actions_tx.clone();
        cameras_button.connect_clicked(move |_| {
            let _ = sender.try_send(UiAction::ShowCameras);
        });
    }
    header.pack_start(&cameras_button);
    header.pack_end(&summary);
    window.set_titlebar(Some(&header));

    let stall_timeout = Duration::from_secs(config.app.stall_timeout_secs);
    // Sem espera por keyframe não há vão longo a tolerar: os dois prazos
    // colapsam num só e o watchdog volta a ser o mais rígido.
    let keyframe_timeout = if config.app.wait_for_keyframe {
        Duration::from_secs(config.app.keyframe_timeout_secs)
    } else {
        stall_timeout
    };
    let spawner = Spawner {
        tokio: bootstrap.tokio.clone(),
        options: PipelineOptions::from_config(&config),
        backoff: Backoff::new(
            config.app.reconnect_initial_secs,
            config.app.reconnect_max_secs,
        ),
        stall_timeout,
        keyframe_timeout,
        redactor: RefCell::new(Redactor::new(&store.devices)),
        recording_options: RecordingOptions::from_config(&config),
        events: events_tx,
        shutdown: shutdown_rx,
        done: RefCell::new(Some(done_tx)),
        actions: actions_tx.clone(),
    };

    let grid = GridView::new(config.app.grid_columns);
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .transition_duration(120)
        .build();
    stack.add_named(&empty_page(&actions_tx), Some(PAGE_EMPTY));
    stack.add_named(&grid.widget(), Some(PAGE_GRID));
    let fullscreen = FullscreenView::new(&actions_tx);
    stack.add_named(fullscreen.widget(), Some(PAGE_SINGLE));
    stack.set_visible_child_name(PAGE_EMPTY);
    window.set_child(Some(&stack));

    if config.notifications.tray {
        crate::notify::spawn_tray(&bootstrap.tokio, tray_rx, tray_cmd_tx);
    }

    let devices = store.devices.clone();
    let dashboard = Rc::new(Dashboard {
        window: window.clone(),
        stack,
        summary,
        fullscreen,
        notifier: Notifier::new(app, &config.notifications),
        grid,
        spawner,
        pipelines: Rc::clone(&bootstrap.pipelines),
        store: RefCell::new(store),
        slots: RefCell::new(Vec::new()),
        quality_prefs: RefCell::new(quality::load()),
        on_cameras_changed: RefCell::new(None),
        snapshot_dir: config.snapshot_dir(),
        adaptive_stream: config.app.adaptive_stream,
        notify_motion: config.motion.enabled && config.motion.notify,
        tray_updates: tray_tx,
        last_tray_summary: RefCell::new(TraySummary::default()),
        config: Rc::clone(&config),
    });

    // Câmeras já cadastradas: entram juntas, com um único cálculo de layout.
    dashboard.grid.set_batch(true);
    for device in &devices {
        // Em variável à parte: o `Ref` temporário viveria durante todo o `for`
        // e faria `spawn_camera` estourar em `borrow_mut`.
        let first_id = dashboard.slots.borrow().len();
        for camera in camera::build_device(first_id, device, &config.app) {
            dashboard.spawn_camera(camera);
        }
    }
    dashboard.grid.set_batch(false);
    dashboard.cameras_changed();

    spawn_loops(&dashboard, events_rx, actions_rx, tray_cmd_rx);
    install_actions(&window, &dashboard);
    install_close_handler(&window, &dashboard, shutdown_tx, done_rx.clone());

    window.present();
    done_rx
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
                UiAction::ShowCameras => manage::show_cameras(&target),
                UiAction::AddManual => manage::show_add_device(&target, None),
                UiAction::ScanNetwork => manage::show_scan(&target),
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
        // Sem novos supervisores: só os que já existem seguram o canal.
        dashboard.spawner.done.borrow_mut().take();
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
                target.open_position(position as usize);
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

//! Widget de uma câmera: vídeo + faixa de identificação + overlay de status.
//!
//! Composição de widgets GTK em vez de subclasse de `GObject` — não precisamos
//! de propriedades/sinais próprios, então a boilerplate não se pagaria. As
//! interações saem por um canal de [`UiAction`], o que evita ciclos `Rc` entre
//! o tile e a janela que o contém.
//!
//! ```text
//! ┌ Overlay ─────────────────────────────────────────────┐
//! │ ● Portão  REC  1920×1080 · 25 fps · 1,8 Mb/s  📷 ⏺   │  ← faixa superior
//! │                                                      │
//! │              [ Picture / paintable ]                 │
//! │              ⟳ Reconectando em 8s                    │  ← overlay central
//! └──────────────────────────────────────────────────────┘
//! ```

use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::pango::EllipsizeMode;
use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::camera::{Camera, Quality};
use crate::pipeline::StreamStats;
use crate::reconnect::CameraState;
use crate::ui::UiAction;

/// Ícone do botão de gravar (câmera de vídeo) e o de parar, usados no card e
/// na tela cheia.
pub const RECORD_ICON: &str = "camera-video-symbolic";
pub const STOP_ICON: &str = "media-playback-stop-symbolic";
/// Alto-falante desligado / ligado, no botão de ouvir o áudio.
pub const LISTEN_OFF_ICON: &str = "audio-volume-muted-symbolic";
pub const LISTEN_ON_ICON: &str = "audio-volume-high-symbolic";

/// Classes CSS mutuamente exclusivas aplicadas ao "LED" de status.
const STATUS_CLASSES: [&str; 3] = ["status-live", "status-connecting", "status-error"];

/// Por quanto tempo o selo de movimento fica aceso depois de uma detecção.
const MOTION_BADGE_DURATION: Duration = Duration::from_secs(6);

pub struct CameraTile {
    root: gtk::Overlay,
    dot: gtk::Label,
    name: gtk::Label,
    detail: gtk::Label,
    rec_badge: gtk::Label,
    motion_badge: gtk::Label,
    record_button: gtk::Button,
    listen_button: gtk::Button,
    center: gtk::Box,
    spinner: gtk::Spinner,
    status: gtk::Label,
    reason: gtk::Label,

    stats: Arc<StreamStats>,
    /// Contadores do último `tick`, para derivar fps e bitrate por diferença.
    last_frames: Cell<u64>,
    last_bytes: Cell<u64>,
    last_tick: Cell<Instant>,
    live: Cell<bool>,
    recording: Cell<bool>,
}

impl CameraTile {
    /// `paintable` é `None` quando a pipeline nem chegou a ser construída — o
    /// tile ainda aparece no grid, só que sem vídeo.
    pub fn new(
        camera: &Camera,
        paintable: Option<&gdk::Paintable>,
        stats: Arc<StreamStats>,
        actions: &async_channel::Sender<UiAction>,
    ) -> Self {
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .hexpand(true)
            .vexpand(true)
            .build();
        picture.set_paintable(paintable);

        let root = gtk::Overlay::builder()
            .css_classes(["camera-tile"])
            .hexpand(true)
            .vexpand(true)
            .child(&picture)
            .build();

        // ---- faixa superior -------------------------------------------------
        let dot = gtk::Label::builder()
            .label("●")
            .css_classes(["status-dot", "status-connecting"])
            .build();

        let name = gtk::Label::builder()
            .label(&camera.name)
            .css_classes(["tile-name"])
            .ellipsize(EllipsizeMode::End)
            .xalign(0.0)
            .hexpand(true)
            .build();
        name.set_tooltip_text(Some(&format!(
            "NVR {} · canal {} · stream {}\n{}",
            camera.nvr_id,
            camera.channel,
            camera.grid_stream,
            camera.masked_url_for(camera.grid_stream)
        )));

        let rec_badge = badge("REC", "badge-rec");
        let motion_badge = badge("MOV", "badge-motion");

        let detail = gtk::Label::builder()
            .label("—")
            .css_classes(["tile-detail"])
            .build();

        // Seletor de qualidade: só existe se o NVR expõe um substream.
        let quality_selector = gtk::DropDown::from_strings(&["Alta", "Baixa"]);
        quality_selector.add_css_class("tile-quality");
        quality_selector.set_tooltip_text(Some("Qualidade da imagem no grid"));
        quality_selector.set_visible(camera.has_substream());
        quality_selector.set_selected(match camera.grid_quality() {
            Quality::High => 0,
            Quality::Low => 1,
        });
        {
            let sender = actions.clone();
            let id = camera.id;
            quality_selector.connect_selected_notify(move |selector| {
                let quality = if selector.selected() == 1 {
                    Quality::Low
                } else {
                    Quality::High
                };
                let _ = sender.try_send(UiAction::SetQuality(id, quality));
            });
        }

        let snapshot_button = tool_button("camera-photo-symbolic", "Capturar PNG (Ctrl+S)");
        let record_button = tool_button(RECORD_ICON, "Gravar (Ctrl+R)");
        let listen_button = tool_button(LISTEN_OFF_ICON, "Ouvir o áudio (Ctrl+M)");
        connect_action(&listen_button, actions, UiAction::ToggleListen(camera.id));
        connect_action(&snapshot_button, actions, UiAction::Snapshot(camera.id));
        connect_action(
            &record_button,
            actions,
            UiAction::ToggleRecording(camera.id),
        );

        let bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .css_classes(["tile-bar"])
            .halign(gtk::Align::Fill)
            .valign(gtk::Align::Start)
            .build();
        for widget in [
            dot.upcast_ref::<gtk::Widget>(),
            name.upcast_ref(),
            rec_badge.upcast_ref(),
            motion_badge.upcast_ref(),
            detail.upcast_ref(),
            quality_selector.upcast_ref(),
            snapshot_button.upcast_ref(),
            listen_button.upcast_ref(),
            record_button.upcast_ref(),
        ] {
            bar.append(widget);
        }
        root.add_overlay(&bar);

        // ---- overlay central (visível apenas quando não há vídeo) -----------
        let spinner = gtk::Spinner::builder()
            .width_request(24)
            .height_request(24)
            .build();
        let status = gtk::Label::builder()
            .label("Conectando…")
            .css_classes(["tile-status"])
            .build();
        let reason = gtk::Label::builder()
            .css_classes(["tile-reason"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .max_width_chars(34)
            .visible(false)
            .build();

        let center = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .css_classes(["tile-center"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        center.append(&spinner);
        center.append(&status);
        center.append(&reason);
        root.add_overlay(&center);

        // ---- clique abre a câmera em tela cheia -----------------------------
        // Os botões da faixa consomem o clique antes de chegar aqui.
        let click = gtk::GestureClick::new();
        let sender = actions.clone();
        let id = camera.id;
        click.connect_released(move |_, _, _, _| {
            let _ = sender.try_send(UiAction::Open(id));
        });
        root.add_controller(click);
        root.set_tooltip_text(Some("Clique para ver em tela cheia"));

        // Tile focável: dá navegação por teclado e serve de alvo para os
        // atalhos Ctrl+S / Ctrl+R enquanto o grid está visível.
        root.set_focusable(true);
        let keys = gtk::EventControllerKey::new();
        let sender = actions.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space) {
                let _ = sender.try_send(UiAction::Open(id));
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        root.add_controller(keys);

        let tile = Self {
            root,
            dot,
            name,
            detail,
            rec_badge,
            motion_badge,
            record_button,
            listen_button,
            center,
            spinner,
            status,
            reason,
            stats,
            last_frames: Cell::new(0),
            last_bytes: Cell::new(0),
            last_tick: Cell::new(Instant::now()),
            live: Cell::new(false),
            recording: Cell::new(false),
        };
        tile.set_state(&CameraState::Connecting);
        tile
    }

    /// Renomeia o card (nome na faixa superior e na dica).
    pub fn set_name(&self, name: &str) {
        self.name.set_label(name);
    }

    /// Última linha "1920×1080 · 25 fps · 1,8 Mb/s" calculada pelo tick, sem
    /// mexer nos contadores (ao contrário de [`sample`](Self::sample)).
    pub fn detail_text(&self) -> String {
        self.detail.label().to_string()
    }

    pub fn widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }

    pub fn is_live(&self) -> bool {
        self.live.get()
    }

    pub fn is_recording(&self) -> bool {
        self.recording.get()
    }

    /// Estatísticas compartilhadas com a pipeline (usadas também no fullscreen).
    pub fn stats(&self) -> &Arc<StreamStats> {
        &self.stats
    }

    /// Reflete uma mudança de estado vinda do supervisor.
    pub fn set_state(&self, state: &CameraState) {
        let (css, spinning, headline, detail) = describe(state);

        for class in STATUS_CLASSES {
            self.dot.remove_css_class(class);
        }
        self.dot.add_css_class(css);

        let live = matches!(state, CameraState::Live);
        self.live.set(live);
        self.center.set_visible(!live);
        self.spinner.set_spinning(spinning && !live);

        if !live {
            self.status.set_label(&headline);
            match detail {
                Some(text) if !text.is_empty() => {
                    self.reason.set_label(&text);
                    self.reason.set_visible(true);
                }
                _ => self.reason.set_visible(false),
            }
            self.detail.set_label("—");
        }
    }

    /// `active` = está gravando de fato; `wanted` = o que o usuário pediu.
    /// Eles divergem enquanto a câmera está fora do ar com gravação pendente.
    /// Reflete se o áudio desta câmera está sendo ouvido.
    pub fn set_listening(&self, on: bool) {
        self.listen_button.set_icon_name(if on {
            LISTEN_ON_ICON
        } else {
            LISTEN_OFF_ICON
        });
        self.listen_button.set_tooltip_text(Some(if on {
            "Parar de ouvir (Ctrl+M)"
        } else {
            "Ouvir o áudio (Ctrl+M)"
        }));
        if on {
            self.listen_button.add_css_class("listening-on");
        } else {
            self.listen_button.remove_css_class("listening-on");
        }
    }

    pub fn set_recording(&self, active: bool, wanted: bool) {
        self.recording.set(active);
        self.rec_badge.set_visible(active);
        self.record_button.set_icon_name(if wanted {
            STOP_ICON
        } else {
            RECORD_ICON
        });
        // Vermelho enquanto grava: o estado salta aos olhos.
        if wanted {
            self.record_button.add_css_class("recording-on");
        } else {
            self.record_button.remove_css_class("recording-on");
        }
        self.record_button.set_tooltip_text(Some(if wanted {
            "Parar gravação (Ctrl+R)"
        } else {
            "Gravar (Ctrl+R)"
        }));
    }

    /// Atualiza fps, bitrate e o selo de movimento.
    /// Chamado ~1×/s pelo main loop do GLib.
    ///
    /// `summary` é o resultado de [`sample`](Self::sample), colhido uma única vez
    /// por tick pelo chamador: amostrar de novo aqui zeraria a janela de
    /// medição e devolveria o texto antigo.
    pub fn tick(&self, summary: &str) {
        self.motion_badge
            .set_visible(self.stats.motion_recent(MOTION_BADGE_DURATION));
        if self.live.get() {
            self.detail.set_label(summary);
        }
    }

    /// Consome os contadores e devolve a linha "1920×1080 · 25 fps · 1,8 Mb/s".
    ///
    /// Tem efeito colateral (zera a janela de medição), por isso é chamado uma
    /// vez por tick e o resultado é reaproveitado pelo fullscreen.
    pub fn sample(&self) -> String {
        let frames = self.stats.frames();
        let bytes = self.stats.bytes();
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick.get()).as_secs_f64();

        let frame_delta = frames.saturating_sub(self.last_frames.get());
        let byte_delta = bytes.saturating_sub(self.last_bytes.get());
        self.last_frames.set(frames);
        self.last_bytes.set(bytes);
        self.last_tick.set(now);

        if elapsed <= 0.1 {
            return self.detail.label().to_string();
        }
        let fps = frame_delta as f64 / elapsed;
        let mbits = (byte_delta as f64 * 8.0) / elapsed / 1_000_000.0;

        match self.stats.resolution() {
            Some((width, height)) => format!("{width}×{height} · {fps:.0} fps · {mbits:.1} Mb/s"),
            None => format!("{fps:.0} fps · {mbits:.1} Mb/s"),
        }
    }
}

/// Traduz um estado em (classe CSS, girar spinner, título, detalhe).
pub(crate) fn describe(state: &CameraState) -> (&'static str, bool, String, Option<String>) {
    match state {
        CameraState::Connecting => ("status-connecting", true, "Conectando…".to_string(), None),
        CameraState::WaitingKeyframe => (
            "status-connecting",
            true,
            "Aguardando keyframe…".to_string(),
            Some(
                "O stream já está chegando. O NVR só manda um quadro completo a \
                 cada GOP — reduza o intervalo de I-frame nas configurações dele \
                 para o vídeo aparecer mais rápido."
                    .to_string(),
            ),
        ),
        CameraState::Live => ("status-live", false, String::new(), None),
        CameraState::Reconnecting {
            attempt,
            retry_in,
            reason,
            nvr_reachable,
        } => {
            let headline = format!(
                "Reconectando em {}s (tentativa {attempt})",
                retry_in.as_secs().max(1)
            );
            let detail = if *nvr_reachable {
                reason.clone()
            } else {
                format!("NVR não responde na rede — {reason}")
            };
            ("status-error", true, headline, Some(detail))
        }
        CameraState::Failed(reason) => (
            "status-error",
            false,
            "Falha".to_string(),
            Some(reason.clone()),
        ),
    }
}

fn badge(text: &str, css_class: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["badge", css_class])
        .visible(false)
        .build()
}

fn tool_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .css_classes(["tile-tool"])
        .has_frame(false)
        .build()
}

fn connect_action(
    button: &gtk::Button,
    actions: &async_channel::Sender<UiAction>,
    action: UiAction,
) {
    let sender = actions.clone();
    button.connect_clicked(move |_| {
        let _ = sender.try_send(action);
    });
}

/// Folha de estilo do dashboard, carregada uma vez no display padrão.
pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));

    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    } else {
        tracing::warn!("nenhum display GDK disponível; CSS não aplicado");
    }
}

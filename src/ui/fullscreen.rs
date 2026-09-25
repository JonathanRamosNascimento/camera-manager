//! View de câmera única, aberta ao clicar num tile do grid.
//!
//! Não movemos o widget do tile para cá: um `GdkPaintable` pode ser desenhado
//! por vários widgets ao mesmo tempo, então basta apontar um `gtk::Picture`
//! maior para o mesmo paintable. Isso evita reparentar widgets (e o risco de
//! derrubar a pipeline no caminho) e mantém o grid intacto por baixo.
//!
//! Com `adaptive_stream` ligado, quem faz a troca para o stream principal é a
//! janela (`ui::mod`), mandando `Command::UseStream` para o supervisor.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, pango};

use crate::camera::Camera;
use crate::reconnect::CameraState;
use crate::ui::{UiAction, camera_tile};
use crate::ui::camera_tile::describe;

const STATUS_CLASSES: [&str; 3] = ["status-live", "status-connecting", "status-error"];

pub struct FullscreenView {
    root: gtk::Box,
    picture: gtk::Picture,
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
    /// Câmera exibida agora; os botões da barra inferior leem daqui.
    current: Rc<Cell<Option<usize>>>,
    /// Mantém o paintable vivo enquanto está sendo exibido.
    paintable: RefCell<Option<gdk::Paintable>>,
}

impl FullscreenView {
    pub fn new(actions: &async_channel::Sender<UiAction>) -> Self {
        let current = Rc::new(Cell::new(None::<usize>));

        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .hexpand(true)
            .vexpand(true)
            .build();

        let overlay = gtk::Overlay::builder()
            .css_classes(["camera-tile", "fullscreen-stage"])
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
            .css_classes(["tile-name", "fullscreen-name"])
            .ellipsize(pango::EllipsizeMode::End)
            .xalign(0.0)
            .hexpand(true)
            .build();
        let rec_badge = badge("REC", "badge-rec");
        let motion_badge = badge("MOV", "badge-motion");
        let detail = gtk::Label::builder()
            .label("—")
            .css_classes(["tile-detail"])
            .build();

        let bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
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
        ] {
            bar.append(widget);
        }
        overlay.add_overlay(&bar);

        // ---- overlay central ------------------------------------------------
        let spinner = gtk::Spinner::builder()
            .width_request(32)
            .height_request(32)
            .build();
        let status = gtk::Label::builder()
            .label("Conectando…")
            .css_classes(["tile-status"])
            .build();
        let reason = gtk::Label::builder()
            .css_classes(["tile-reason"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .max_width_chars(48)
            .visible(false)
            .build();
        let center = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .css_classes(["tile-center"])
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        center.append(&spinner);
        center.append(&status);
        center.append(&reason);
        overlay.add_overlay(&center);

        // ---- barra inferior -------------------------------------------------
        let back = gtk::Button::builder()
            .label("Voltar ao grid")
            .icon_name("go-previous-symbolic")
            .tooltip_text("Esc")
            .build();
        let sender = actions.clone();
        back.connect_clicked(move |_| {
            let _ = sender.try_send(UiAction::Back);
        });

        let snapshot_button = gtk::Button::builder()
            .icon_name("camera-photo-symbolic")
            .label("Capturar")
            .tooltip_text("Salva o quadro atual em PNG (Ctrl+S)")
            .build();
        let record_button = gtk::Button::builder()
            .icon_name(camera_tile::RECORD_ICON)
            .label("Gravar")
            .tooltip_text("Inicia/para a gravação (Ctrl+R)")
            .build();
        let listen_button = gtk::Button::builder()
            .icon_name(camera_tile::LISTEN_OFF_ICON)
            .label("Ouvir")
            .tooltip_text("Ouve o áudio da câmera (Ctrl+M)")
            .build();
        bind_current(&listen_button, actions, &current, UiAction::ToggleListen);
        bind_current(&snapshot_button, actions, &current, UiAction::Snapshot);
        bind_current(&record_button, actions, &current, UiAction::ToggleRecording);

        let toolbar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["fullscreen-toolbar"])
            .build();
        toolbar.append(&back);
        let spacer = gtk::Box::builder().hexpand(true).build();
        toolbar.append(&spacer);
        toolbar.append(&snapshot_button);
        toolbar.append(&listen_button);
        toolbar.append(&record_button);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .css_classes(["fullscreen-view"])
            .build();
        root.append(&overlay);
        root.append(&toolbar);

        Self {
            root,
            picture,
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
            current,
            paintable: RefCell::new(None),
        }
    }

    pub fn widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }

    /// Qual câmera está sendo exibida.
    pub fn current(&self) -> Option<usize> {
        self.current.get()
    }

    /// Passa a exibir `camera`, reaproveitando o paintable da pipeline dela.
    pub fn show(&self, camera: &Camera, paintable: Option<&gdk::Paintable>) {
        self.current.set(Some(camera.id));
        self.picture.set_paintable(paintable);
        *self.paintable.borrow_mut() = paintable.cloned();
        self.name.set_label(&format!(
            "{}  ·  NVR {} · canal {}",
            camera.name, camera.nvr_id, camera.channel
        ));
        self.detail.set_label("—");
    }

    /// Solta o paintable ao voltar para o grid, para não segurar referências.
    pub fn clear(&self) {
        self.current.set(None);
        self.picture.set_paintable(gdk::Paintable::NONE);
        *self.paintable.borrow_mut() = None;
    }

    pub fn set_state(&self, state: &CameraState) {
        let (css, spinning, headline, detail) = describe(state);

        for class in STATUS_CLASSES {
            self.dot.remove_css_class(class);
        }
        self.dot.add_css_class(css);

        let live = matches!(state, CameraState::Live);
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

    pub fn set_detail(&self, text: &str) {
        self.detail.set_label(text);
    }

    pub fn set_recording(&self, recording: bool) {
        self.rec_badge.set_visible(recording);
        self.record_button.set_icon_name(if recording {
            camera_tile::STOP_ICON
        } else {
            camera_tile::RECORD_ICON
        });
        if recording {
            self.record_button.add_css_class("recording-on");
        } else {
            self.record_button.remove_css_class("recording-on");
        }
        self.record_button
            .set_label(if recording { "Parar" } else { "Gravar" });
    }

    pub fn set_listening(&self, on: bool) {
        self.listen_button.set_icon_name(if on {
            camera_tile::LISTEN_ON_ICON
        } else {
            camera_tile::LISTEN_OFF_ICON
        });
        self.listen_button
            .set_label(if on { "Silenciar" } else { "Ouvir" });
        if on {
            self.listen_button.add_css_class("listening-on");
        } else {
            self.listen_button.remove_css_class("listening-on");
        }
    }

    pub fn set_motion(&self, active: bool) {
        self.motion_badge.set_visible(active);
    }
}

fn badge(text: &str, css_class: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["badge", css_class])
        .visible(false)
        .build()
}

/// Liga um botão a uma ação que depende da câmera exibida no momento.
fn bind_current(
    button: &gtk::Button,
    actions: &async_channel::Sender<UiAction>,
    current: &Rc<Cell<Option<usize>>>,
    make: fn(usize) -> UiAction,
) {
    let sender = actions.clone();
    let current = Rc::clone(current);
    button.connect_clicked(move |_| {
        if let Some(id) = current.get() {
            let _ = sender.try_send(make(id));
        }
    });
}

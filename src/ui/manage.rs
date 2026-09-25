//! Janelas de gerenciamento de câmeras: lista, cadastro manual e varredura.
//!
//! Tudo aqui roda na thread do GTK. O que é lento (varredura de rede, teste de
//! canais) vai para o runtime do tokio e volta por canais assíncronos, então a
//! interface nunca trava esperando a rede.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::rc::{Rc, Weak};

use gtk::prelude::*;
use gtk::{gio, glib};

use super::{Dashboard, Slot};
use crate::camera::Quality;
use crate::config::{DEFAULT_URL_TEMPLATE, Secret};
use crate::discovery::{self, Found, ProbeResult, ScanEvent};
use crate::reconnect::CameraState;
use crate::store::{ChannelEntry, DEFAULT_RTSP_PORT, Device, parse_channels};

/// Canais testados por "Detectar canais".
const DETECT_CHANNELS: u32 = 8;

thread_local! {
    /// Uma janela de cada tipo por vez: abrir de novo só a traz para a frente.
    static CAMERAS_WINDOW: RefCell<Option<glib::WeakRef<gtk::Window>>> = const { RefCell::new(None) };
    static SCAN_WINDOW: RefCell<Option<glib::WeakRef<gtk::Window>>> = const { RefCell::new(None) };
}

/// Traz para a frente a janela guardada em `slot`, se ainda existir.
fn present_existing(
    slot: &'static std::thread::LocalKey<RefCell<Option<glib::WeakRef<gtk::Window>>>>,
) -> bool {
    slot.with(|cell| {
        if let Some(window) = cell
            .borrow()
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .filter(|window| window.is_visible())
        {
            window.present();
            true
        } else {
            false
        }
    })
}

fn remember(
    slot: &'static std::thread::LocalKey<RefCell<Option<glib::WeakRef<gtk::Window>>>>,
    window: &gtk::Window,
) {
    slot.with(|cell| *cell.borrow_mut() = Some(window.downgrade()));
    // Janela fechada = janela esquecida: senão o botão "reabriria" a antiga,
    // já destruída, em vez de criar uma nova.
    window.connect_destroy(move |_| slot.with(|cell| *cell.borrow_mut() = None));
}

/// Esc fecha a janela.
fn close_on_escape(window: &gtk::Window) {
    let keys = gtk::EventControllerKey::new();
    let weak = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape
            && let Some(window) = weak.upgrade()
        {
            window.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);
}

/// Janela em primeiro plano, para as novas ficarem "presas" nela.
fn parent_of(dash: &Dashboard) -> gtk::Window {
    dash.window
        .application()
        .and_then(|app| app.active_window())
        .unwrap_or_else(|| dash.window.clone().upcast())
}

fn dialog(dash: &Dashboard, title: &str, width: i32, height: i32, modal: bool) -> gtk::Window {
    let window = gtk::Window::builder()
        .title(title)
        .transient_for(&parent_of(dash))
        .modal(modal)
        .destroy_with_parent(true)
        .default_width(width)
        .default_height(height)
        .build();
    close_on_escape(&window);
    window
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .css_classes(classes.to_vec())
        .build()
}

fn content_box(spacing: i32) -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(spacing)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build()
}

fn pluralize(n: usize) -> String {
    if n == 1 {
        "1 câmera".to_string()
    } else {
        format!("{n} câmeras")
    }
}

// ---------------------------------------------------------------------------
// Lista de câmeras
// ---------------------------------------------------------------------------

/// Rótulos de uma linha da lista que mudam com o tempo (status, fps…).
struct LiveRow {
    id: usize,
    dot: gtk::Label,
    info: gtk::Label,
}

pub(super) fn show_cameras(dash: &Rc<Dashboard>) {
    if present_existing(&CAMERAS_WINDOW) {
        return;
    }
    let window = dialog(dash, "Gerenciar câmeras", 640, 520, false);
    remember(&CAMERAS_WINDOW, &window);

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.set_placeholder(Some(&label(
        "Nenhuma câmera cadastrada.\nUse os botões abaixo para adicionar.",
        &["dim-label"],
    )));
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list)
        .build();

    let buttons = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    let scan = gtk::Button::with_label("Escanear a rede");
    scan.add_css_class("suggested-action");
    let manual = gtk::Button::with_label("Adicionar manualmente");
    buttons.append(&manual);
    buttons.append(&scan);

    let root = content_box(12);
    root.append(&scroller);
    root.append(&buttons);
    window.set_child(Some(&root));

    {
        let dash = Rc::downgrade(dash);
        scan.connect_clicked(move |_| {
            if let Some(dash) = dash.upgrade() {
                show_scan(&dash);
            }
        });
    }
    {
        let dash = Rc::downgrade(dash);
        manual.connect_clicked(move |_| {
            if let Some(dash) = dash.upgrade() {
                show_add_device(&dash, None);
            }
        });
    }

    // Reconstrói a lista quando o conjunto de câmeras muda...
    let live: Rc<RefCell<Vec<LiveRow>>> = Rc::default();
    let refresh = {
        let dash = Rc::downgrade(dash);
        let list = list.clone();
        let window = window.downgrade();
        let live = Rc::clone(&live);
        move || {
            if let (Some(dash), Some(window)) = (dash.upgrade(), window.upgrade()) {
                fill_camera_list(&list, &dash, &window, &live);
            }
        }
    };
    refresh();
    *dash.on_cameras_changed.borrow_mut() = Some(Box::new(refresh));

    // ...e atualiza status/resolução/fps a cada segundo, sem refazer as linhas
    // (refazer tiraria o foco dos botões).
    {
        let dash = Rc::downgrade(dash);
        let window = window.downgrade();
        glib::timeout_add_seconds_local(1, move || {
            let (Some(dash), Some(_)) = (dash.upgrade(), window.upgrade()) else {
                return glib::ControlFlow::Break;
            };
            for row in live.borrow().iter() {
                if let Some(slot) = dash.slot(row.id) {
                    let (css, text) = live_status(&slot);
                    set_status_class(&row.dot, css);
                    row.info.set_label(&text);
                }
            }
            glib::ControlFlow::Continue
        });
    }
    {
        let dash = Rc::downgrade(dash);
        window.connect_destroy(move |_| {
            if let Some(dash) = dash.upgrade() {
                *dash.on_cameras_changed.borrow_mut() = None;
            }
        });
    }
    window.present();
}

const STATUS_CLASSES: [&str; 3] = ["status-live", "status-connecting", "status-error"];

fn set_status_class(dot: &gtk::Label, css: &str) {
    for class in STATUS_CLASSES {
        dot.remove_css_class(class);
    }
    dot.add_css_class(css);
}

/// Classe de cor e texto de estado de uma câmera: "Ao vivo · 1920×1080 · 25 fps…".
fn live_status(slot: &Slot) -> (&'static str, String) {
    let (css, state) = match &*slot.state.borrow() {
        CameraState::Live => ("status-live", "Ao vivo".to_string()),
        CameraState::Connecting => ("status-connecting", "Conectando…".to_string()),
        CameraState::WaitingKeyframe => ("status-connecting", "Aguardando keyframe…".to_string()),
        CameraState::Reconnecting { attempt, .. } => (
            "status-error",
            format!("Reconectando (tentativa {attempt})"),
        ),
        CameraState::Failed(reason) => ("status-error", format!("Falha: {reason}")),
    };
    let mut text = state;
    if slot.tile.is_live() {
        text.push_str(&format!(" · {}", slot.tile.detail_text()));
    }
    if slot.camera.has_substream() {
        text.push_str(match slot.quality.get() {
            Quality::High => " · qualidade alta",
            Quality::Low => " · qualidade baixa",
        });
    }
    (css, text)
}

fn fill_camera_list(
    list: &gtk::ListBox,
    dash: &Rc<Dashboard>,
    window: &gtk::Window,
    live: &Rc<RefCell<Vec<LiveRow>>>,
) {
    list.remove_all();
    live.borrow_mut().clear();

    for slot in dash.live_slots() {
        let camera = &slot.camera;
        let username = dash
            .store
            .borrow()
            .devices
            .iter()
            .find(|d| d.id == camera.nvr_id)
            .map(|d| d.username.clone())
            .unwrap_or_default();

        let dot = gtk::Label::builder()
            .label("●")
            .css_classes(["status-dot"])
            .valign(gtk::Align::Start)
            .margin_top(2)
            .build();
        let name = label(&slot.name.borrow(), &["heading"]);
        let address = label(
            &format!(
                "{}:{} · canal {} · usuário {username}",
                camera.host, camera.port, camera.channel
            ),
            &["dim-label", "caption"],
        );
        let (css, status_text) = live_status(&slot);
        set_status_class(&dot, css);
        let info = label(&status_text, &["caption"]);

        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        text.append(&name);
        text.append(&address);
        text.append(&info);

        let edit = gtk::Button::builder()
            .label("Editar")
            .valign(gtk::Align::Center)
            .build();
        let remove = gtk::Button::builder()
            .label("Remover")
            .valign(gtk::Align::Center)
            .css_classes(["destructive-action"])
            .build();
        let id = camera.id;
        {
            let dash = Rc::downgrade(dash);
            edit.connect_clicked(move |_| {
                if let Some(dash) = dash.upgrade() {
                    show_edit_camera(&dash, id);
                }
            });
        }
        {
            // Fraca: o botão vive dentro da janela; uma referência forte formaria
            // um ciclo e a janela nunca seria liberada.
            let dash = Rc::downgrade(dash);
            let window = window.downgrade();
            let name = slot.name.borrow().clone();
            remove.connect_clicked(move |_| {
                if let Some(window) = window.upgrade() {
                    confirm_remove(&window, &dash, id, &name);
                }
            });
        }

        let row = gtk::Box::builder()
            .spacing(12)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        row.append(&dot);
        row.append(&text);
        row.append(&edit);
        row.append(&remove);
        list.append(&row);
        live.borrow_mut().push(LiveRow { id, dot, info });
    }
}

fn confirm_remove(window: &gtk::Window, dash: &Weak<Dashboard>, id: usize, name: &str) {
    let alert = gtk::AlertDialog::builder()
        .message(format!("Remover “{name}”?"))
        .detail("A câmera sai do dashboard e do cadastro. Para usá-la de novo, será preciso adicioná-la outra vez.")
        .buttons(["Cancelar", "Remover"])
        .cancel_button(0)
        .default_button(0)
        .modal(true)
        .build();
    let dash = dash.clone();
    alert.choose(Some(window), gio::Cancellable::NONE, move |answer| {
        if answer == Ok(1)
            && let Some(dash) = dash.upgrade()
        {
            dash.remove_camera(id);
        }
    });
}

// ---------------------------------------------------------------------------
// Edição
// ---------------------------------------------------------------------------

/// Edita o nome da câmera e os dados de conexão do dispositivo dela.
pub(super) fn show_edit_camera(dash: &Rc<Dashboard>, id: usize) {
    let Some(slot) = dash.slot(id) else {
        return;
    };
    let Some(device) = dash
        .store
        .borrow()
        .devices
        .iter()
        .find(|d| d.id == slot.camera.nvr_id)
        .cloned()
    else {
        return;
    };
    let siblings = device.channels.len();

    let window = dialog(dash, "Editar câmera", 460, -1, true);
    let name = entry(&slot.name.borrow(), "Nome");
    let host = entry(&device.host, "192.168.1.10");
    let port = entry(&device.port.to_string(), "554");
    let user = entry(&device.username, "usuário");
    let password = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .hexpand(true)
        .placeholder_text("deixe vazio para manter a atual")
        .activates_default(true)
        .build();
    let template = entry(
        device.url_template.as_deref().unwrap_or(""),
        DEFAULT_URL_TEMPLATE,
    );

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build();
    let rows: [(&str, &gtk::Widget); 5] = [
        ("Nome", name.upcast_ref()),
        ("Endereço (IP)", host.upcast_ref()),
        ("Porta RTSP", port.upcast_ref()),
        ("Usuário", user.upcast_ref()),
        ("Senha", password.upcast_ref()),
    ];
    for (row, (text, widget)) in rows.iter().enumerate() {
        let caption = label(text, &[]);
        caption.set_valign(gtk::Align::Center);
        grid.attach(&caption, 0, row as i32, 1, 1);
        grid.attach(*widget, 1, row as i32, 1, 1);
    }

    let shared_note = label(
        &format!(
            "Endereço, porta, usuário e senha valem para o dispositivo inteiro \
             ({}). Canal {}.",
            if siblings == 1 {
                "1 câmera".to_string()
            } else {
                format!("{siblings} câmeras")
            },
            slot.camera.channel
        ),
        &["dim-label", "caption"],
    );
    let advanced = gtk::Expander::builder().label("Avançado").build();
    let advanced_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(6)
        .build();
    advanced_box.append(&label(
        "Modelo da URL RTSP. Vazio = padrão iCSee/XMEye.",
        &["dim-label", "caption"],
    ));
    advanced_box.append(&template);
    advanced.set_child(Some(&advanced_box));
    let status = label("", &["error-text"]);

    let cancel = gtk::Button::with_label("Cancelar");
    let save = gtk::Button::with_label("Salvar");
    save.add_css_class("suggested-action");
    window.set_default_widget(Some(&save));
    let buttons = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::End)
        .margin_top(6)
        .build();
    buttons.append(&cancel);
    buttons.append(&save);

    let root = content_box(10);
    for widget in [
        grid.upcast_ref::<gtk::Widget>(),
        shared_note.upcast_ref(),
        advanced.upcast_ref(),
        status.upcast_ref(),
        buttons.upcast_ref(),
    ] {
        root.append(widget);
    }
    window.set_child(Some(&root));

    {
        // Fraca: o botão vive dentro da janela (forte formaria um ciclo).
        let window = window.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.close();
            }
        });
    }
    {
        let dash = Rc::downgrade(dash);
        let window = window.downgrade();
        let status = status.clone();
        let name = name.clone();
        save.connect_clicked(move |_| {
            let Some(dash) = dash.upgrade() else { return };
            status.set_label("");

            let port_text = port.text();
            let Ok(new_port) = port_text.trim().parse::<u16>() else {
                status.set_label(&format!("porta inválida: \"{}\"", port_text.trim()));
                return;
            };
            let new_template = template.text().trim().to_string();
            let new_template = (!new_template.is_empty() && new_template != DEFAULT_URL_TEMPLATE)
                .then_some(new_template);
            let new_password = password.text().to_string();
            let new_password = (!new_password.is_empty()).then(|| Secret::new(new_password));

            let connection_changed = host.text().trim() != device.host
                || new_port != device.port
                || user.text().trim() != device.username
                || new_password.is_some()
                || new_template != device.url_template;

            // O nome primeiro: se a conexão mudar, as câmeras são recriadas já
            // com o nome novo.
            dash.rename_camera(id, &name.text());
            if connection_changed
                && let Err(err) = dash.update_device(
                    &device.id,
                    host.text().trim(),
                    new_port,
                    user.text().trim(),
                    new_password,
                    new_template,
                )
            {
                status.set_label(&format!("{err:#}"));
                return;
            }
            if let Some(window) = window.upgrade() {
                window.close();
            }
        });
    }
    window.present();
    name.grab_focus();
}

// ---------------------------------------------------------------------------
// Cadastro manual
// ---------------------------------------------------------------------------

/// Valores para pré-preencher o formulário (vindos da varredura).
pub(super) struct Prefill {
    pub host: String,
    pub port: u16,
    pub name: Option<String>,
}

/// Nome de cada canal: o do dispositivo, com o número quando há vários.
fn channel_name(base: &str, channel: u32, total: usize) -> String {
    if total == 1 {
        base.to_string()
    } else {
        format!("{base} {channel}")
    }
}

/// Campos do formulário, para ler e para o "Detectar canais" reaproveitar.
#[derive(Clone)]
struct Form {
    name: gtk::Entry,
    host: gtk::Entry,
    port: gtk::Entry,
    user: gtk::Entry,
    password: gtk::PasswordEntry,
    channels: gtk::Entry,
    template: gtk::Entry,
}

impl Form {
    /// Login e endereço, sem exigir canais — o bastante para testar.
    fn read_connection(&self) -> anyhow::Result<Device> {
        let host = self.host.text().trim().to_string();
        let port_text = self.port.text();
        let port = if port_text.trim().is_empty() {
            DEFAULT_RTSP_PORT
        } else {
            port_text
                .trim()
                .parse::<u16>()
                .map_err(|_| anyhow::anyhow!("porta inválida: \"{}\"", port_text.trim()))?
        };
        let template = self.template.text().trim().to_string();
        let device = Device {
            id: Device::make_id(&host, port),
            name: self.name.text().trim().to_string(),
            host,
            port,
            username: self.user.text().trim().to_string(),
            password: Secret::new(self.password.text().to_string()),
            url_template: (!template.is_empty() && template != DEFAULT_URL_TEMPLATE)
                .then_some(template),
            channels: Vec::new(),
        };
        // Reaproveita as mesmas mensagens do cadastro para os campos básicos.
        let mut probe = device.clone();
        probe.channels.push(ChannelEntry {
            channel: 1,
            name: String::new(),
            stream: 0,
        });
        probe.validate()?;
        Ok(device)
    }

    fn read(&self) -> anyhow::Result<Device> {
        let mut device = self.read_connection()?;
        let channels = parse_channels(&self.channels.text())?;
        let base = if device.name.is_empty() {
            "Câmera".to_string()
        } else {
            device.name.clone()
        };
        let total = channels.len();
        device.channels = channels
            .into_iter()
            .map(|channel| ChannelEntry {
                channel,
                name: channel_name(&base, channel, total),
                stream: 0,
            })
            .collect();
        device.name = base;
        Ok(device)
    }
}

fn entry(text: &str, placeholder: &str) -> gtk::Entry {
    gtk::Entry::builder()
        .text(text)
        .placeholder_text(placeholder)
        .hexpand(true)
        .activates_default(true)
        .build()
}

pub(super) fn show_add_device(dash: &Rc<Dashboard>, prefill: Option<Prefill>) {
    let window = dialog(dash, "Adicionar câmera", 460, -1, true);

    let form = Form {
        name: entry(
            prefill
                .as_ref()
                .and_then(|p| p.name.as_deref())
                .unwrap_or("Câmera"),
            "Câmera",
        ),
        host: entry(
            prefill.as_ref().map_or("", |p| p.host.as_str()),
            "192.168.1.10",
        ),
        port: entry(
            &prefill
                .as_ref()
                .map_or(DEFAULT_RTSP_PORT, |p| p.port)
                .to_string(),
            "554",
        ),
        user: entry("admin", "usuário"),
        password: gtk::PasswordEntry::builder()
            .show_peek_icon(true)
            .hexpand(true)
            .activates_default(true)
            .build(),
        channels: entry("1", "ex.: 1-4 ou 1,3"),
        template: entry("", DEFAULT_URL_TEMPLATE),
    };

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .build();
    let rows: [(&str, &gtk::Widget); 6] = [
        ("Nome", form.name.upcast_ref()),
        ("Endereço (IP)", form.host.upcast_ref()),
        ("Porta RTSP", form.port.upcast_ref()),
        ("Usuário", form.user.upcast_ref()),
        ("Senha", form.password.upcast_ref()),
        ("Canais", form.channels.upcast_ref()),
    ];
    for (row, (text, widget)) in rows.iter().enumerate() {
        let caption = label(text, &[]);
        caption.set_valign(gtk::Align::Center);
        grid.attach(&caption, 0, row as i32, 1, 1);
        grid.attach(*widget, 1, row as i32, 1, 1);
    }

    let advanced = gtk::Expander::builder().label("Avançado").build();
    let advanced_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(6)
        .build();
    advanced_box.append(&label(
        "Modelo da URL RTSP. Vazio = padrão iCSee/XMEye. Use {host} {port} {channel} {stream} \
         {user} {password} {user_enc} {password_enc}.",
        &["dim-label", "caption"],
    ));
    advanced_box.append(&form.template);
    advanced.set_child(Some(&advanced_box));

    let hint = label(
        "Num NVR, “Detectar canais” testa os canais 1 a 8 e marca só os que têm imagem.",
        &["dim-label", "caption"],
    );
    let results = label("", &["caption"]);
    results.set_selectable(true);
    let status = label("", &["error-text"]);

    let detect = gtk::Button::with_label("Detectar canais");
    let cancel = gtk::Button::with_label("Cancelar");
    let add = gtk::Button::with_label("Adicionar");
    add.add_css_class("suggested-action");
    window.set_default_widget(Some(&add));

    let buttons = gtk::Box::builder().spacing(8).margin_top(6).build();
    buttons.append(&detect);
    let spacer = gtk::Box::builder().hexpand(true).build();
    buttons.append(&spacer);
    buttons.append(&cancel);
    buttons.append(&add);

    let root = content_box(10);
    for widget in [
        grid.upcast_ref::<gtk::Widget>(),
        hint.upcast_ref(),
        advanced.upcast_ref(),
        results.upcast_ref(),
        status.upcast_ref(),
        buttons.upcast_ref(),
    ] {
        root.append(widget);
    }
    window.set_child(Some(&root));

    {
        // Fraca: o botão vive dentro da janela (forte formaria um ciclo).
        let window = window.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.close();
            }
        });
    }

    {
        let dash = Rc::clone(dash);
        let form = form.clone();
        let status = status.clone();
        let window = window.downgrade();
        add.connect_clicked(move |_| {
            status.set_label("");
            let device = match form.read() {
                Ok(device) => device,
                Err(err) => {
                    status.set_label(&format!("{err:#}"));
                    return;
                }
            };
            match dash.add_device(device) {
                Ok(0) => {
                    dash.toast("Essas câmeras já estavam cadastradas");
                    if let Some(window) = window.upgrade() {
                        window.close();
                    }
                }
                Ok(count) => {
                    dash.toast(&format!("{} adicionada(s)", pluralize(count)));
                    if let Some(window) = window.upgrade() {
                        window.close();
                    }
                }
                Err(err) => status.set_label(&format!("{err:#}")),
            }
        });
    }

    {
        let dash = Rc::clone(dash);
        let form = form.clone();
        let status = status.clone();
        let results = results.clone();
        let button = detect.clone();
        detect.connect_clicked(move |_| {
            status.set_label("");
            results.set_label("");
            let device = match form.read_connection() {
                Ok(device) => device,
                Err(err) => {
                    status.set_label(&format!("{err:#}"));
                    return;
                }
            };
            detect_channels(&dash, device, &form, &results, &status, &button);
        });
    }

    window.present();
    form.host.grab_focus();
}

/// Testa os canais 1..=8 e preenche o campo "Canais" com os que têm imagem.
fn detect_channels(
    dash: &Rc<Dashboard>,
    device: Device,
    form: &Form,
    results: &gtk::Label,
    status: &gtk::Label,
    button: &gtk::Button,
) {
    button.set_sensitive(false);
    button.set_label("Testando…");

    let (tx, rx) = async_channel::unbounded();
    dash.spawner.tokio.spawn(discovery::probe_channels(
        device,
        (1..=DETECT_CHANNELS).collect(),
        tx,
    ));

    let form = form.clone();
    let results = results.clone();
    let status = status.clone();
    let button = button.clone();
    glib::spawn_future_local(async move {
        let mut seen: BTreeMap<u32, ProbeResult> = BTreeMap::new();
        while let Ok((channel, result)) = rx.recv().await {
            seen.insert(channel, result);
            results.set_label(&probe_report(&seen));
        }

        let with_video: Vec<String> = seen
            .iter()
            .filter(|(_, r)| r.is_video())
            .map(|(channel, _)| channel.to_string())
            .collect();
        if seen.values().any(|r| *r == ProbeResult::Unauthorized) {
            status.set_label("Usuário ou senha recusados pelo dispositivo.");
        } else if with_video.is_empty() {
            status.set_label(
                "Nenhum canal com imagem. Confira o endereço/porta e se as câmeras estão \
                 ligadas e gravando no NVR.",
            );
        } else {
            form.channels.set_text(&with_video.join(","));
        }
        button.set_label("Detectar canais");
        button.set_sensitive(true);
    });
}

fn probe_report(seen: &BTreeMap<u32, ProbeResult>) -> String {
    seen.iter()
        .map(|(channel, result)| {
            let mark = if result.is_video() { "✔" } else { "✖" };
            format!("{mark} Canal {channel} — {}", result.describe())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Varredura de rede
// ---------------------------------------------------------------------------

pub(super) fn show_scan(dash: &Rc<Dashboard>) {
    if present_existing(&SCAN_WINDOW) {
        return;
    }
    let window = dialog(dash, "Escanear a rede", 560, 520, false);
    remember(&SCAN_WINDOW, &window);

    let prefix = discovery::local_prefix();
    let subnet = entry(
        &prefix.map_or(String::new(), |[a, b, c]| format!("{a}.{b}.{c}.0/24")),
        "192.168.1.0/24",
    );
    let start = gtk::Button::with_label("Escanear");
    start.add_css_class("suggested-action");
    let top = gtk::Box::builder().spacing(8).build();
    top.append(&label("Sub-rede", &[]));
    top.append(&subnet);
    top.append(&start);

    let progress = gtk::ProgressBar::builder().visible(false).build();
    let status = label(
        "Procura dispositivos com a porta RTSP (554) aberta e câmeras ONVIF.",
        &["dim-label", "caption"],
    );
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&list)
        .build();

    let root = content_box(10);
    for widget in [
        top.upcast_ref::<gtk::Widget>(),
        progress.upcast_ref(),
        status.upcast_ref(),
        scroller.upcast_ref(),
    ] {
        root.append(widget);
    }
    window.set_child(Some(&root));

    let scanning = Rc::new(Cell::new(false));
    let closed = Rc::new(Cell::new(false));
    {
        let closed = Rc::clone(&closed);
        window.connect_close_request(move |_| {
            closed.set(true);
            glib::Propagation::Proceed
        });
    }

    let run = {
        let dash = Rc::downgrade(dash);
        let (subnet, start, progress, status, list) = (
            subnet.clone(),
            start.clone(),
            progress.clone(),
            status.clone(),
            list.clone(),
        );
        Rc::new(move || {
            let Some(dash) = dash.upgrade() else { return };
            if scanning.get() {
                return;
            }
            let prefix = match discovery::parse_prefix(&subnet.text()) {
                Ok(prefix) => prefix,
                Err(err) => {
                    status.set_label(&format!("{err:#}"));
                    return;
                }
            };
            scanning.set(true);
            start.set_sensitive(false);
            progress.set_visible(true);
            progress.set_fraction(0.0);
            status.set_label("Escaneando…");
            list.remove_all();

            let (tx, rx) = async_channel::unbounded();
            dash.spawner
                .tokio
                .spawn(discovery::scan(prefix, DEFAULT_RTSP_PORT, tx));

            let (scanning, closed) = (Rc::clone(&scanning), Rc::clone(&closed));
            let (start, progress, status, list) = (
                start.clone(),
                progress.clone(),
                status.clone(),
                list.clone(),
            );
            let dash = Rc::downgrade(&dash);
            glib::spawn_future_local(async move {
                let mut found: BTreeMap<Ipv4Addr, Found> = BTreeMap::new();
                // Sair do loop larga `rx`; a varredura percebe e para sozinha.
                while let Ok(event) = rx.recv().await {
                    if closed.get() {
                        break;
                    }
                    match event {
                        ScanEvent::Progress { done, total } => {
                            progress.set_fraction(done as f64 / total as f64);
                            status.set_label(&format!("Verificando {done}/{total} endereços…"));
                        }
                        ScanEvent::Found(item) => {
                            found.insert(item.ip, item);
                            if let Some(dash) = dash.upgrade() {
                                fill_scan_list(&list, &dash, &found);
                            }
                        }
                        ScanEvent::Finished => break,
                    }
                }
                if !closed.get() {
                    progress.set_visible(false);
                    status.set_label(&scan_summary(found.len(), prefix));
                }
                start.set_sensitive(true);
                scanning.set(false);
            });
        })
    };

    {
        let run = Rc::clone(&run);
        start.connect_clicked(move |_| run());
    }
    {
        let run = Rc::clone(&run);
        subnet.connect_activate(move |_| run());
    }
    window.present();
    // Já com a sub-rede da máquina descoberta, não faz sentido pedir outro clique.
    if prefix.is_some() {
        run();
    }
}

fn scan_summary(count: usize, prefix: [u8; 3]) -> String {
    let net = format!("{}.{}.{}.0/24", prefix[0], prefix[1], prefix[2]);
    match count {
        0 => format!(
            "Nada encontrado em {net}. Confira a sub-rede e se as câmeras/NVR estão ligados \
             nessa mesma rede. Você ainda pode adicionar manualmente."
        ),
        1 => {
            "1 dispositivo encontrado. Clique em “Adicionar” e informe usuário e senha.".to_string()
        }
        n => format!(
            "{n} dispositivos encontrados. Clique em “Adicionar” e informe usuário e senha."
        ),
    }
}

fn fill_scan_list(list: &gtk::ListBox, dash: &Rc<Dashboard>, found: &BTreeMap<Ipv4Addr, Found>) {
    list.remove_all();
    let registered: Vec<String> = dash
        .store
        .borrow()
        .devices
        .iter()
        .map(|d| d.host.clone())
        .collect();

    for item in found.values() {
        let ip = item.ip.to_string();
        let mut details = Vec::new();
        details.push(if item.rtsp_open {
            "RTSP aberto (porta 554)".to_string()
        } else {
            "porta RTSP fechada".to_string()
        });
        if let Some(name) = &item.onvif {
            details.push(format!("ONVIF: {name}"));
        }
        if registered.contains(&ip) {
            details.push("já cadastrado".to_string());
        }

        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        text.append(&label(&ip, &["heading"]));
        text.append(&label(&details.join(" · "), &["dim-label", "caption"]));

        let add = gtk::Button::builder()
            .label("Adicionar…")
            .valign(gtk::Align::Center)
            .build();
        let prefill = Prefill {
            host: ip,
            port: DEFAULT_RTSP_PORT,
            name: item.onvif.clone(),
        };
        let dash = Rc::downgrade(dash);
        add.connect_clicked(move |_| {
            if let Some(dash) = dash.upgrade() {
                show_add_device(
                    &dash,
                    Some(Prefill {
                        host: prefill.host.clone(),
                        port: prefill.port,
                        name: prefill.name.clone(),
                    }),
                );
            }
        });

        let row = gtk::Box::builder()
            .spacing(12)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        row.append(&text);
        row.append(&add);
        list.append(&row);
    }
}

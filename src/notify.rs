//! Notificações do desktop e ícone na bandeja do sistema.
//!
//! São dois caminhos diferentes:
//! - **notificações**: `gio::Notification` pela `GtkApplication`, que no GNOME
//!   cai direto no centro de notificações — funciona sem nenhuma extensão;
//! - **bandeja**: `StatusNotifierItem` via `ksni`, rodando numa task do tokio.
//!   O GNOME só mostra ícones de bandeja com a extensão AppIndicator instalada;
//!   sem ela o serviço sobe e simplesmente não aparece nada.
//!
//! O ícone da bandeja fala com a UI por um canal (`TrayCommand`), porque os
//! callbacks do `ksni` rodam fora da thread do GTK.

use std::cell::RefCell;
use std::collections::HashSet;

use gtk::gio;
use gtk::prelude::*;
#[cfg(target_os = "linux")]
use ksni::TrayMethods;

/// Ícone usado na bandeja e nas notificações (tema Adwaita).
const ICON_NAME: &str = "camera-video-symbolic";

// ---------------------------------------------------------------------------
// Notificações do desktop
// ---------------------------------------------------------------------------

/// Emite notificações do desktop a partir dos eventos das câmeras.
///
/// Vive na thread do GTK, daí o `RefCell` em vez de `Mutex`.
pub struct Notifier {
    app: gtk::Application,
    enabled: bool,
    /// Só avisamos a partir desta tentativa de reconexão, para não alarmar
    /// com uma queda de um segundo.
    offline_after_attempts: u32,
    /// Câmeras que já geraram um aviso de "offline" e ainda não voltaram.
    notified_offline: RefCell<HashSet<usize>>,
}

impl Notifier {
    pub fn new(app: &gtk::Application, config: &crate::config::Notifications) -> Self {
        Self {
            app: app.clone(),
            enabled: config.enabled,
            offline_after_attempts: config.offline_after_attempts.max(1),
            notified_offline: RefCell::new(HashSet::new()),
        }
    }

    /// Uma câmera falhou em reconectar. Notifica uma única vez por queda.
    pub fn camera_offline(&self, camera_id: usize, name: &str, attempt: u32, reason: &str) {
        if !self.enabled || attempt < self.offline_after_attempts {
            return;
        }
        if !self.notified_offline.borrow_mut().insert(camera_id) {
            return;
        }
        let notification = gio::Notification::new("Câmera offline");
        notification.set_body(Some(&format!("{name} — {reason}")));
        notification.set_priority(gio::NotificationPriority::High);
        notification.set_icon(&gio::ThemedIcon::new(ICON_NAME));
        self.app
            .send_notification(Some(&offline_id(camera_id)), &notification);
    }

    /// A câmera voltou: retira o aviso e confirma a recuperação.
    pub fn camera_recovered(&self, camera_id: usize, name: &str) {
        if !self.notified_offline.borrow_mut().remove(&camera_id) {
            return;
        }
        self.app.withdraw_notification(&offline_id(camera_id));
        if !self.enabled {
            return;
        }
        let notification = gio::Notification::new("Câmera de volta");
        notification.set_body(Some(&format!("{name} voltou a transmitir")));
        notification.set_priority(gio::NotificationPriority::Low);
        notification.set_icon(&gio::ThemedIcon::new(ICON_NAME));
        self.app.send_notification(None, &notification);
    }

    pub fn motion(&self, name: &str) {
        let notification = gio::Notification::new("Movimento detectado");
        notification.set_body(Some(name));
        notification.set_priority(gio::NotificationPriority::Normal);
        notification.set_icon(&gio::ThemedIcon::new(ICON_NAME));
        self.app.send_notification(None, &notification);
    }

    pub fn recording_failed(&self, name: &str, reason: &str) {
        if !self.enabled {
            return;
        }
        let notification = gio::Notification::new("Falha na gravação");
        notification.set_body(Some(&format!("{name} — {reason}")));
        notification.set_priority(gio::NotificationPriority::High);
        notification.set_icon(&gio::ThemedIcon::new(ICON_NAME));
        self.app.send_notification(None, &notification);
    }
}

fn offline_id(camera_id: usize) -> String {
    format!("camera-offline-{camera_id}")
}

// ---------------------------------------------------------------------------
// Bandeja do sistema
// ---------------------------------------------------------------------------

/// Resumo mostrado no tooltip da bandeja.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TraySummary {
    pub total: usize,
    pub live: usize,
    pub recording: usize,
    /// Nomes das câmeras que não estão ao vivo.
    pub offline: Vec<String>,
}

/// O que o menu da bandeja pede à janela.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// Trazer a janela para a frente.
    Present,
    Quit,
}

#[cfg(target_os = "linux")]
struct DashboardTray {
    summary: TraySummary,
    commands: async_channel::Sender<TrayCommand>,
}

#[cfg(target_os = "linux")]
impl DashboardTray {
    fn send(&self, command: TrayCommand) {
        // `try_send` não bloqueia; se a UI já sumiu, não há o que fazer.
        let _ = self.commands.try_send(command);
    }
}

#[cfg(target_os = "linux")]
impl ksni::Tray for DashboardTray {
    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").into()
    }

    fn title(&self) -> String {
        "NVR Dashboard".into()
    }

    fn icon_name(&self) -> String {
        ICON_NAME.into()
    }

    /// `NeedsAttention` faz o ícone se destacar quando alguma câmera cai.
    fn status(&self) -> ksni::Status {
        if self.summary.offline.is_empty() {
            ksni::Status::Active
        } else {
            ksni::Status::NeedsAttention
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let mut description = format!(
            "{}/{} câmeras ao vivo",
            self.summary.live, self.summary.total
        );
        if self.summary.recording > 0 {
            description.push_str(&format!("\n{} gravando", self.summary.recording));
        }
        if !self.summary.offline.is_empty() {
            description.push_str(&format!("\nOffline: {}", self.summary.offline.join(", ")));
        }
        ksni::ToolTip {
            icon_name: ICON_NAME.into(),
            title: "NVR Dashboard".into(),
            description,
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCommand::Present);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            ksni::menu::StandardItem {
                label: "Abrir dashboard".into(),
                icon_name: ICON_NAME.into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Present)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            ksni::menu::StandardItem {
                label: "Sair".into(),
                icon_name: "application-exit-symbolic".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayCommand::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Sobe o ícone da bandeja numa task do tokio e mantém o resumo atualizado.
///
/// Falhar aqui não é fatal: sem um servidor de `StatusNotifierItem` (GNOME sem
/// a extensão AppIndicator, por exemplo) o dashboard segue funcionando normal.
#[cfg(target_os = "linux")]
pub fn spawn_tray(
    tokio: &tokio::runtime::Handle,
    updates: async_channel::Receiver<TraySummary>,
    commands: async_channel::Sender<TrayCommand>,
) {
    tokio.spawn(async move {
        let tray = DashboardTray {
            summary: TraySummary::default(),
            commands,
        };
        let handle = match tray.spawn().await {
            Ok(handle) => {
                tracing::info!("ícone da bandeja registrado");
                handle
            }
            Err(err) => {
                tracing::warn!(
                    erro = %err,
                    "sem ícone na bandeja (no GNOME, exige a extensão AppIndicator)"
                );
                return;
            }
        };

        while let Ok(summary) = updates.recv().await {
            if handle.update(|tray| tray.summary = summary).await.is_none() {
                break;
            }
        }
        handle.shutdown().await;
        tracing::debug!("serviço da bandeja encerrado");
    });
}

/// Fora do Linux não há `StatusNotifierItem`: o ícone da bandeja não existe e o
/// resto do app (incluindo as notificações) funciona igual.
#[cfg(not(target_os = "linux"))]
pub fn spawn_tray(
    _tokio: &tokio::runtime::Handle,
    _updates: async_channel::Receiver<TraySummary>,
    _commands: async_channel::Sender<TrayCommand>,
) {
    tracing::info!("ícone de bandeja indisponível neste sistema (só Linux)");
}

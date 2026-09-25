//! Cadastro de dispositivos (NVRs / câmeras IP) feito pela interface.
//!
//! Fica em `<config>/nvr-dashboard/devices.toml`, com permissão `600` porque
//! guarda credenciais — o mesmo cuidado que o `cameras.toml` sempre teve. O
//! `cameras.toml` passa a guardar só ajustes do app.
//!
//! Um dispositivo tem um host, credenciais e uma lista de canais; cada canal
//! vira um card no grid.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{DEFAULT_URL_TEMPLATE, Secret};

pub const DEFAULT_RTSP_PORT: u16 = 554;
const FILE_NAME: &str = "devices.toml";

/// Um canal (uma câmera) de um dispositivo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelEntry {
    pub channel: u32,
    pub name: String,
    /// `0` = stream principal, `1` = substream.
    #[serde(default)]
    pub stream: u8,
}

/// Um NVR ou câmera IP: um endereço, um login, vários canais.
#[derive(Clone, Serialize, Deserialize)]
pub struct Device {
    /// Identificador estável: `host:porta`.
    pub id: String,
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub username: String,
    pub password: Secret,
    /// `None` = template padrão do firmware iCSee/XMEye.
    #[serde(default)]
    pub url_template: Option<String>,
    #[serde(default)]
    pub channels: Vec<ChannelEntry>,
}

fn default_port() -> u16 {
    DEFAULT_RTSP_PORT
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Secret` já mascara a senha.
        f.debug_struct("Device")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password)
            .field("channels", &self.channels)
            .finish()
    }
}

impl Device {
    pub fn make_id(host: &str, port: u16) -> String {
        format!("{}:{port}", host.trim())
    }

    pub fn template(&self) -> &str {
        self.url_template.as_deref().unwrap_or(DEFAULT_URL_TEMPLATE)
    }

    /// Campos obrigatórios preenchidos e canais sem repetição.
    pub fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() {
            bail!("informe o endereço (IP ou host) do dispositivo");
        }
        if self.username.trim().is_empty() {
            bail!("informe o usuário");
        }
        if self.password.is_empty() {
            bail!("informe a senha");
        }
        if self.port == 0 {
            bail!("a porta precisa ser maior que 0");
        }
        if self.channels.is_empty() {
            bail!("escolha ao menos um canal");
        }
        let mut seen = std::collections::HashSet::new();
        if let Some(dup) = self.channels.iter().find(|c| !seen.insert(c.channel)) {
            bail!("canal {} repetido", dup.channel);
        }
        Ok(())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    devices: Vec<Device>,
}

/// Dispositivos cadastrados + o arquivo onde eles vivem.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    pub devices: Vec<Device>,
}

impl Store {
    pub fn default_path() -> Option<PathBuf> {
        crate::config::user_config_dir().map(|dir| dir.join("nvr-dashboard").join(FILE_NAME))
    }

    /// Lê o cadastro. Arquivo ausente = lista vazia (primeira execução).
    ///
    /// Se o arquivo existir mas estiver ilegível, é movido para `.bak` antes de
    /// começar vazio: assim o próximo `save` não destrói o que o usuário tinha.
    pub fn load(path: PathBuf) -> Self {
        let devices = match fs::read_to_string(&path) {
            Ok(raw) => match toml::from_str::<File>(&raw) {
                Ok(file) => file.devices,
                Err(err) => {
                    let backup = path.with_extension("toml.bak");
                    tracing::error!(
                        arquivo = %path.display(),
                        backup = %backup.display(),
                        %err,
                        "cadastro de câmeras ilegível; movido para backup"
                    );
                    let _ = fs::rename(&path, &backup);
                    Vec::new()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                tracing::error!(arquivo = %path.display(), %err, "não consegui ler o cadastro");
                Vec::new()
            }
        };
        Self { path, devices }
    }

    /// Grava com permissão `600`, de forma atômica (arquivo temporário + rename).
    pub fn save(&self) -> Result<()> {
        let dir = self.path.parent().context("caminho sem diretório")?;
        fs::create_dir_all(dir).with_context(|| format!("criando {}", dir.display()))?;

        let text = toml::to_string_pretty(&File {
            devices: self.devices.clone(),
        })?;
        let tmp = self.path.with_extension("toml.tmp");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("gravando {}", tmp.display()))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        // `mode` só vale na criação; garante 600 mesmo se o tmp já existia.
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Adiciona um dispositivo. Se o `host:porta` já existe, atualiza as
    /// credenciais e acrescenta só os canais que ainda não estavam lá.
    /// Devolve os canais efetivamente novos.
    pub fn add(&mut self, device: Device) -> Result<Vec<ChannelEntry>> {
        device.validate()?;
        match self.devices.iter_mut().find(|d| d.id == device.id) {
            Some(existing) => {
                existing.username = device.username;
                existing.password = device.password;
                existing.url_template = device.url_template;
                let mut added = Vec::new();
                for entry in device.channels {
                    if !existing.channels.iter().any(|c| c.channel == entry.channel) {
                        existing.channels.push(entry.clone());
                        added.push(entry);
                    }
                }
                existing.channels.sort_by_key(|c| c.channel);
                Ok(added)
            }
            None => {
                let added = device.channels.clone();
                self.devices.push(device);
                Ok(added)
            }
        }
    }

    /// Troca endereço, porta, usuário, senha e modelo de URL de um dispositivo,
    /// mantendo os canais. `password = None` mantém a senha atual.
    ///
    /// O `id` acompanha `host:porta`; se o novo já pertencer a outro
    /// dispositivo, falha sem alterar nada.
    pub fn update_device(
        &mut self,
        old_id: &str,
        host: &str,
        port: u16,
        username: &str,
        password: Option<Secret>,
        url_template: Option<String>,
    ) -> Result<Device> {
        let Some(index) = self.devices.iter().position(|d| d.id == old_id) else {
            bail!("dispositivo não encontrado");
        };
        let new_id = Device::make_id(host, port);
        if new_id != old_id && self.devices.iter().any(|d| d.id == new_id) {
            bail!("já existe um dispositivo em {new_id}");
        }
        let mut updated = self.devices[index].clone();
        updated.id = new_id;
        updated.host = host.trim().to_string();
        updated.port = port;
        updated.username = username.trim().to_string();
        if let Some(password) = password {
            updated.password = password;
        }
        updated.url_template = url_template;
        updated.channels = self.devices[index].channels.clone();
        // Mesmas regras do cadastro (host/usuário/senha preenchidos, etc.).
        updated.validate()?;
        self.devices[index] = updated.clone();
        Ok(updated)
    }

    /// Renomeia um canal.
    pub fn rename_channel(&mut self, device_id: &str, channel: u32, name: &str) {
        if let Some(entry) = self
            .devices
            .iter_mut()
            .find(|d| d.id == device_id)
            .and_then(|d| d.channels.iter_mut().find(|c| c.channel == channel))
        {
            entry.name = name.trim().to_string();
        }
    }

    /// Remove um canal; o dispositivo some junto quando fica sem canais.
    pub fn remove_channel(&mut self, device_id: &str, channel: u32) {
        if let Some(device) = self.devices.iter_mut().find(|d| d.id == device_id) {
            device.channels.retain(|c| c.channel != channel);
        }
        self.devices.retain(|d| !d.channels.is_empty());
    }
}

/// Interpreta uma lista de canais como `1-3`, `1,2,5` ou `1-3,6`.
pub fn parse_channels(text: &str) -> Result<Vec<u32>> {
    let mut channels = Vec::new();
    for part in text.split([',', ';', ' ']).filter(|p| !p.trim().is_empty()) {
        let part = part.trim();
        let (start, end) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse::<u32>(), b.trim().parse::<u32>()),
            None => {
                let n = part.parse::<u32>();
                (n.clone(), n)
            }
        };
        let (Ok(start), Ok(end)) = (start, end) else {
            bail!("canais inválidos: \"{part}\" (use algo como 1-3 ou 1,2,5)");
        };
        if start == 0 || end < start || end > 64 {
            bail!("canais inválidos: \"{part}\" (de 1 a 64)");
        }
        channels.extend(start..=end);
    }
    channels.sort_unstable();
    channels.dedup();
    if channels.is_empty() {
        bail!("informe os canais (ex.: 1-3)");
    }
    Ok(channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(host: &str, channels: &[u32]) -> Device {
        Device {
            id: Device::make_id(host, 554),
            name: "NVR".into(),
            host: host.into(),
            port: 554,
            username: "admin".into(),
            password: Secret::new("segredo"),
            url_template: None,
            channels: channels
                .iter()
                .map(|&channel| ChannelEntry {
                    channel,
                    name: format!("Câmera {channel}"),
                    stream: 0,
                })
                .collect(),
        }
    }

    fn store(dir: &std::path::Path) -> Store {
        Store::load(dir.join("devices.toml"))
    }

    #[test]
    fn canais_aceitam_faixas_e_listas() {
        assert_eq!(parse_channels("1-3").unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_channels("1, 3,5-6").unwrap(), vec![1, 3, 5, 6]);
        assert_eq!(parse_channels("2 2").unwrap(), vec![2]);
        assert!(parse_channels("").is_err());
        assert!(parse_channels("0").is_err());
        assert!(parse_channels("3-1").is_err());
        assert!(parse_channels("a").is_err());
    }

    #[test]
    fn primeira_execucao_nao_tem_dispositivos() {
        let dir = tempdir();
        assert!(store(&dir).devices.is_empty());
    }

    #[test]
    fn grava_com_permissao_600_e_le_de_volta() {
        let dir = tempdir();
        let mut s = store(&dir);
        s.add(device("10.0.0.5", &[1, 2])).unwrap();
        s.save().unwrap();

        let mode = fs::metadata(dir.join("devices.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        let back = store(&dir);
        assert_eq!(back.devices.len(), 1);
        assert_eq!(back.devices[0].channels.len(), 2);
        assert_eq!(back.devices[0].password.expose(), "segredo");
    }

    #[test]
    fn adicionar_o_mesmo_host_so_acrescenta_canais_novos() {
        let dir = tempdir();
        let mut s = store(&dir);
        s.add(device("10.0.0.5", &[1, 2])).unwrap();
        let added = s.add(device("10.0.0.5", &[2, 3])).unwrap();
        assert_eq!(s.devices.len(), 1);
        assert_eq!(added.iter().map(|c| c.channel).collect::<Vec<_>>(), vec![3]);
        assert_eq!(s.devices[0].channels.len(), 3);
    }

    #[test]
    fn remover_o_ultimo_canal_remove_o_dispositivo() {
        let dir = tempdir();
        let mut s = store(&dir);
        s.add(device("10.0.0.5", &[1, 2])).unwrap();
        s.remove_channel("10.0.0.5:554", 1);
        assert_eq!(s.devices[0].channels.len(), 1);
        s.remove_channel("10.0.0.5:554", 2);
        assert!(s.devices.is_empty());
    }

    #[test]
    fn editar_dispositivo_troca_ip_e_mantem_canais_e_senha() {
        let dir = tempdir();
        let mut s = store(&dir);
        s.add(device("10.0.0.5", &[1, 2])).unwrap();
        let updated = s
            .update_device("10.0.0.5:554", "10.0.0.9", 8554, "operador", None, None)
            .unwrap();
        assert_eq!(updated.id, "10.0.0.9:8554");
        assert_eq!(updated.channels.len(), 2);
        assert_eq!(updated.password.expose(), "segredo", "senha mantida");
        assert_eq!(s.devices[0].username, "operador");
    }

    #[test]
    fn editar_para_endereco_de_outro_dispositivo_falha_sem_alterar() {
        let dir = tempdir();
        let mut s = store(&dir);
        s.add(device("10.0.0.5", &[1])).unwrap();
        s.add(device("10.0.0.6", &[1])).unwrap();
        let err = s
            .update_device("10.0.0.5:554", "10.0.0.6", 554, "admin", None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("já existe"), "{err}");
        assert_eq!(s.devices[0].host, "10.0.0.5");
    }

    #[test]
    fn editar_com_senha_nova_e_renomear_canal() {
        let dir = tempdir();
        let mut s = store(&dir);
        s.add(device("10.0.0.5", &[1, 2])).unwrap();
        let updated = s
            .update_device(
                "10.0.0.5:554",
                "10.0.0.5",
                554,
                "admin",
                Some(Secret::new("nova")),
                None,
            )
            .unwrap();
        assert_eq!(updated.password.expose(), "nova");
        s.rename_channel("10.0.0.5:554", 2, "  Quintal ");
        assert_eq!(s.devices[0].channels[1].name, "Quintal");
    }

    #[test]
    fn validacao_recusa_campos_vazios() {
        let mut d = device("10.0.0.5", &[1]);
        d.password = Secret::new("");
        assert!(d.validate().is_err());
        let mut d = device("", &[1]);
        d.host = "  ".into();
        assert!(d.validate().is_err());
        assert!(device("10.0.0.5", &[]).validate().is_err());
    }

    #[test]
    fn arquivo_corrompido_vai_para_backup() {
        let dir = tempdir();
        fs::write(dir.join("devices.toml"), "isto não é toml [[[").unwrap();
        let s = store(&dir);
        assert!(s.devices.is_empty());
        assert!(dir.join("devices.toml.bak").exists());
        assert!(!dir.join("devices.toml").exists());
    }

    #[test]
    fn debug_nao_vaza_a_senha() {
        let text = format!("{:?}", device("10.0.0.5", &[1]));
        assert!(!text.contains("segredo"));
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nvr-store-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}

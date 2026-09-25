//! Carregamento, validação e proteção do arquivo de configuração TOML.
//!
//! O arquivo real (`config/cameras.toml`) carrega as credenciais do NVR e por isso:
//! - fica no `.gitignore`;
//! - deve ter permissão `600` (checada em [`Config::load`]);
//! - a senha é embrulhada em [`Secret`], que nunca imprime o valor em claro.

use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gtk::glib;
use serde::Deserialize;

/// Template padrão de URL RTSP do NVR iCSee/XMEye (firmware Hi3520).
pub const DEFAULT_URL_TEMPLATE: &str = "rtsp://{user_enc}:{password_enc}@{host}:{port}\
     /user={user}&password={password}&channel={channel}&stream={stream}.sdp";

/// Texto que substitui a senha em qualquer saída legível por humanos.
pub const MASK: &str = "***";

/// Nome do arquivo de configuração procurado nos diretórios padrão.
pub const CONFIG_FILE_NAME: &str = "cameras.toml";

/// Variável de ambiente que sobrescreve a busca por caminhos padrão.
pub const CONFIG_ENV_VAR: &str = "NVR_DASHBOARD_CONFIG";

/// Subdiretório criado dentro de Imagens/Vídeos para os arquivos gerados.
const OUTPUT_SUBDIR: &str = "nvr-dashboard";

// ---------------------------------------------------------------------------
// Secret
// ---------------------------------------------------------------------------

/// String sensível: `Debug` e `Display` sempre imprimem [`MASK`].
///
/// O valor em claro só sai por [`Secret::expose`], o que torna trivial auditar
/// (`grep expose()`) todos os pontos em que a senha é realmente usada.
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Devolve a senha em claro. Use apenas para montar a URL entregue ao GStreamer.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(MASK)
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(MASK)
    }
}

// ---------------------------------------------------------------------------
// Estruturas de configuração
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Forma simples: um único gravador.
    #[serde(default)]
    pub nvr: Option<Nvr>,
    /// Forma múltipla: vários gravadores no mesmo dashboard.
    #[serde(default)]
    pub nvrs: Vec<Nvr>,
    #[serde(default)]
    pub app: App,
    #[serde(default)]
    pub cameras: Vec<CameraEntry>,
    #[serde(default)]
    pub snapshots: Snapshots,
    #[serde(default)]
    pub recording: Recording,
    #[serde(default)]
    pub motion: Motion,
    #[serde(default)]
    pub notifications: Notifications,
}

/// Dados de conexão de um gravador.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Nvr {
    /// Identificador citado em `cameras.nvr`. Padrão: o próprio `host`.
    #[serde(default)]
    pub id: Option<String>,
    /// IP ou hostname do NVR na rede local.
    pub host: String,
    #[serde(default = "default_rtsp_port")]
    pub rtsp_port: u16,
    pub username: String,
    pub password: Secret,
    /// Template da URL RTSP. Placeholders suportados:
    /// `{host}`, `{port}`, `{channel}`, `{stream}`,
    /// `{user}` / `{password}` (literais) e
    /// `{user_enc}` / `{password_enc}` (percent-encoded, para o userinfo).
    #[serde(default = "default_url_template")]
    pub url_template: String,
}

impl Nvr {
    /// Identificador efetivo: o `id` explícito ou o host.
    pub fn id(&self) -> &str {
        self.id.as_deref().unwrap_or(&self.host)
    }
}

/// Ajustes de comportamento do dashboard.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    /// Buffer de jitter do `rtspsrc`, em milissegundos.
    #[serde(default = "default_latency_ms")]
    pub latency_ms: u32,
    /// Número de colunas do grid. `None` = calculado a partir do nº de câmeras.
    #[serde(default)]
    pub grid_columns: Option<usize>,
    /// Usa decodificação por hardware (VA-API / NVDEC) quando disponível.
    /// `false` rebaixa esses decoders no registro, forçando software (`avdec_*`).
    ///
    /// O ganho aqui é pequeno (3 streams custam ~0,3 de um núcleo com hardware
    /// contra ~0,4 sem), então `false` é uma saída perfeitamente boa se algum
    /// driver der problema.
    #[serde(default = "default_true")]
    pub hardware_decoding: bool,
    /// Transportes RTSP aceitos, na sintaxe de flags do GStreamer (`tcp`, `udp`,
    /// `tcp+udp`). TCP é mais confiável em Wi-Fi; UDP tem latência menor.
    #[serde(default = "default_rtsp_protocols")]
    pub rtsp_protocols: String,
    /// Sem nenhum **dado** do NVR por este tempo, a conexão é dada como morta.
    /// Depois que o vídeo começa, vale também para quadros parados.
    #[serde(default = "default_stall_timeout_secs")]
    pub stall_timeout_secs: u64,
    /// Segura a exibição até o primeiro keyframe.
    ///
    /// Quem entra no meio de um GOP não tem o quadro de referência: o
    /// decodificador pinta blocos sobre uma superfície zerada e a imagem fica
    /// esverdeada até o próximo keyframe. Com `true` o tile mostra "Aguardando
    /// keyframe…" nesse intervalo, o que é mais honesto. Ponha `false` se
    /// preferir ver a imagem se formando aos poucos.
    #[serde(default = "default_true")]
    pub wait_for_keyframe: bool,
    /// Quanto tempo tolerar "dados chegando, nenhum quadro decodificável".
    ///
    /// Precisa ser maior que o intervalo de I-frame do NVR, senão a pipeline
    /// reinicia antes de alcançar o keyframe — e nunca exibe nada.
    #[serde(default = "default_keyframe_timeout_secs")]
    pub keyframe_timeout_secs: u64,
    /// Primeiro intervalo de espera do backoff exponencial de reconexão.
    #[serde(default = "default_reconnect_initial_secs")]
    pub reconnect_initial_secs: u64,
    /// Teto do backoff exponencial de reconexão.
    #[serde(default = "default_reconnect_max_secs")]
    pub reconnect_max_secs: u64,
    /// Usa o substream no grid e troca para o stream principal no fullscreen.
    #[serde(default)]
    pub adaptive_stream: bool,
    /// Índice do substream usado quando `adaptive_stream` está ligado.
    #[serde(default = "default_substream_index")]
    pub substream_index: u8,
}

/// Uma câmera (um canal de um NVR).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraEntry {
    pub name: String,
    pub channel: u32,
    /// `0` = stream principal (alta qualidade); `1` = substream.
    #[serde(default)]
    pub stream: u8,
    /// `id` do NVR desta câmera. Opcional quando só existe um gravador.
    #[serde(default)]
    pub nvr: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Onde as capturas de tela (PNG) são gravadas.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshots {
    /// Padrão: `<Imagens>/nvr-dashboard`.
    #[serde(default)]
    pub directory: Option<String>,
}

/// Gravação local sob demanda (`splitmuxsink`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recording {
    /// Padrão: `<Vídeos>/nvr-dashboard`.
    #[serde(default)]
    pub directory: Option<String>,
    /// Duração de cada arquivo do buffer circular.
    #[serde(default = "default_segment_seconds")]
    pub segment_seconds: u64,
    /// Quantos segmentos manter por sessão de gravação. `0` = sem limite.
    #[serde(default)]
    pub max_files: u32,
    /// `mkv` (matroskamux, tolerante a interrupção) ou `mp4` (mp4mux).
    #[serde(default = "default_container")]
    pub container: String,
}

/// Detecção de movimento por diferença de quadros.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Motion {
    /// Liga o ramo de análise na pipeline (custa um pouco de CPU por câmera).
    #[serde(default)]
    pub enabled: bool,
    /// Diferença mínima por pixel (0–255) para contá-lo como alterado.
    #[serde(default = "default_motion_threshold")]
    pub threshold: u8,
    /// Fração da imagem que precisa mudar para disparar (0.0–1.0).
    #[serde(default = "default_motion_sensitivity")]
    pub sensitivity: f64,
    /// Tempo mínimo entre dois disparos da mesma câmera.
    #[serde(default = "default_motion_cooldown_secs")]
    pub cooldown_secs: u64,
    /// Manda uma notificação do desktop a cada detecção.
    #[serde(default)]
    pub notify: bool,
}

/// Notificações do desktop e ícone na bandeja.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notifications {
    /// Avisa quando uma câmera fica offline e quando volta.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Só notifica a partir desta tentativa de reconexão, para não alarmar
    /// com quedas de um segundo.
    #[serde(default = "default_offline_after_attempts")]
    pub offline_after_attempts: u32,
    /// Ícone na bandeja (StatusNotifierItem). No GNOME exige a extensão
    /// AppIndicator; sem ela o ícone simplesmente não aparece.
    #[serde(default = "default_true")]
    pub tray: bool,
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

impl Default for App {
    fn default() -> Self {
        Self {
            latency_ms: default_latency_ms(),
            grid_columns: None,
            hardware_decoding: true,
            rtsp_protocols: default_rtsp_protocols(),
            stall_timeout_secs: default_stall_timeout_secs(),
            wait_for_keyframe: true,
            keyframe_timeout_secs: default_keyframe_timeout_secs(),
            reconnect_initial_secs: default_reconnect_initial_secs(),
            reconnect_max_secs: default_reconnect_max_secs(),
            adaptive_stream: false,
            substream_index: default_substream_index(),
        }
    }
}

impl Default for Recording {
    fn default() -> Self {
        Self {
            directory: None,
            segment_seconds: default_segment_seconds(),
            max_files: 0,
            container: default_container(),
        }
    }
}

impl Default for Motion {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_motion_threshold(),
            sensitivity: default_motion_sensitivity(),
            cooldown_secs: default_motion_cooldown_secs(),
            notify: false,
        }
    }
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            enabled: true,
            offline_after_attempts: default_offline_after_attempts(),
            tray: true,
        }
    }
}

fn default_rtsp_port() -> u16 {
    554
}
fn default_url_template() -> String {
    DEFAULT_URL_TEMPLATE.to_string()
}
fn default_latency_ms() -> u32 {
    200
}
fn default_rtsp_protocols() -> String {
    "tcp".to_string()
}
fn default_stall_timeout_secs() -> u64 {
    12
}
fn default_keyframe_timeout_secs() -> u64 {
    90
}
fn default_reconnect_initial_secs() -> u64 {
    2
}
fn default_reconnect_max_secs() -> u64 {
    60
}
fn default_substream_index() -> u8 {
    1
}
fn default_segment_seconds() -> u64 {
    300
}
fn default_container() -> String {
    "mkv".to_string()
}
fn default_motion_threshold() -> u8 {
    24
}
fn default_motion_sensitivity() -> f64 {
    0.02
}
fn default_motion_cooldown_secs() -> u64 {
    10
}
fn default_offline_after_attempts() -> u32 {
    2
}
fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Carregamento
// ---------------------------------------------------------------------------

impl Config {
    /// Lê e valida o TOML no caminho informado.
    ///
    /// Emite um aviso (sem falhar) se o arquivo estiver legível por outros
    /// usuários, já que ele guarda a senha do NVR.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("não consegui ler {}", path.display()))?;

        warn_on_loose_permissions(path);

        let config: Config =
            toml::from_str(&raw).with_context(|| format!("TOML inválido em {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Descobre qual arquivo de configuração usar.
    ///
    /// Ordem: caminho explícito (`--config`) → `$NVR_DASHBOARD_CONFIG` →
    /// `./config/cameras.toml` → `$XDG_CONFIG_HOME/nvr-dashboard/cameras.toml`.
    pub fn resolve_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
        if let Some(path) = explicit {
            if !path.is_file() {
                bail!("arquivo de configuração não encontrado: {}", path.display());
            }
            return Ok(path);
        }

        let mut candidates = Vec::new();
        if let Some(from_env) = std::env::var_os(CONFIG_ENV_VAR) {
            candidates.push(PathBuf::from(from_env));
        }
        candidates.push(PathBuf::from("config").join(CONFIG_FILE_NAME));
        if let Some(dir) = user_config_dir() {
            candidates.push(dir.join(OUTPUT_SUBDIR).join(CONFIG_FILE_NAME));
        }

        if let Some(found) = candidates.iter().find(|p| p.is_file()) {
            return Ok(found.clone());
        }

        let tried = candidates
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "nenhum arquivo de configuração encontrado. Caminhos tentados:\n{tried}\n\n\
             Copie o template e preencha as credenciais:\n  \
             cp config/cameras.example.toml config/{CONFIG_FILE_NAME}\n  \
             chmod 600 config/{CONFIG_FILE_NAME}"
        );
    }

    /// Todos os gravadores, na ordem: `[nvr]` primeiro, depois `[[nvrs]]`.
    pub fn all_nvrs(&self) -> Vec<&Nvr> {
        self.nvr.iter().chain(self.nvrs.iter()).collect()
    }

    /// Localiza o gravador de uma câmera.
    ///
    /// Sem `nvr = "..."` explícito só há resposta quando existe um único
    /// gravador — com vários, omitir seria ambíguo.
    pub fn nvr_for(&self, entry: &CameraEntry) -> Result<&Nvr> {
        let nvrs = self.all_nvrs();
        match &entry.nvr {
            Some(id) => nvrs
                .into_iter()
                .find(|nvr| nvr.id() == id)
                .with_context(|| {
                    format!(
                        "câmera `{}` referencia o NVR `{id}`, que não existe",
                        entry.name
                    )
                }),
            None if nvrs.len() == 1 => Ok(nvrs[0]),
            None => bail!(
                "câmera `{}` precisa de `nvr = \"<id>\"`: há {} gravadores configurados",
                entry.name,
                nvrs.len()
            ),
        }
    }

    fn validate(&self) -> Result<()> {
        let nvrs = self.all_nvrs();
        if nvrs.is_empty() {
            bail!("nenhum gravador configurado — adicione um bloco `[nvr]`");
        }

        let mut seen = Vec::with_capacity(nvrs.len());
        for nvr in &nvrs {
            if nvr.host.trim().is_empty() {
                bail!("`nvr.host` não pode ser vazio");
            }
            if nvr.username.trim().is_empty() {
                bail!("`nvr.username` não pode ser vazio (NVR `{}`)", nvr.id());
            }
            if nvr.password.is_empty() {
                bail!("`nvr.password` não pode ser vazio (NVR `{}`)", nvr.id());
            }
            if nvr.url_template.trim().is_empty() {
                bail!("`nvr.url_template` não pode ser vazio (NVR `{}`)", nvr.id());
            }
            if seen.contains(&nvr.id()) {
                bail!("dois gravadores com o mesmo id `{}`", nvr.id());
            }
            seen.push(nvr.id());
        }

        if self.app.reconnect_initial_secs == 0 {
            bail!("`app.reconnect_initial_secs` precisa ser >= 1");
        }
        if self.app.reconnect_max_secs < self.app.reconnect_initial_secs {
            bail!("`app.reconnect_max_secs` precisa ser >= `app.reconnect_initial_secs`");
        }
        if self.app.stall_timeout_secs == 0 {
            bail!("`app.stall_timeout_secs` precisa ser >= 1");
        }
        if self.app.keyframe_timeout_secs < self.app.stall_timeout_secs {
            bail!("`app.keyframe_timeout_secs` precisa ser >= `app.stall_timeout_secs`");
        }
        if let Some(0) = self.app.grid_columns {
            bail!("`app.grid_columns` precisa ser >= 1 (ou omitido para automático)");
        }
        if self.recording.segment_seconds == 0 {
            bail!("`recording.segment_seconds` precisa ser >= 1");
        }
        if !matches!(self.recording.container.as_str(), "mkv" | "mp4") {
            bail!(
                "`recording.container` deve ser \"mkv\" ou \"mp4\", não {:?}",
                self.recording.container
            );
        }
        if !(0.0..=1.0).contains(&self.motion.sensitivity) {
            bail!("`motion.sensitivity` precisa estar entre 0.0 e 1.0");
        }

        if self.enabled_cameras().next().is_none() {
            bail!("nenhuma câmera habilitada — adicione ao menos um bloco `[[cameras]]`");
        }
        for camera in self.enabled_cameras() {
            if camera.name.trim().is_empty() {
                bail!("câmera do canal {} está sem `name`", camera.channel);
            }
            self.nvr_for(camera)?;
        }
        Ok(())
    }

    pub fn enabled_cameras(&self) -> impl Iterator<Item = &CameraEntry> {
        self.cameras.iter().filter(|c| c.enabled)
    }

    /// Diretório das capturas PNG, com `~` expandido.
    pub fn snapshot_dir(&self) -> PathBuf {
        resolve_output_dir(&self.snapshots.directory, glib::UserDirectory::Pictures)
    }

    /// Diretório das gravações, com `~` expandido.
    pub fn recording_dir(&self) -> PathBuf {
        resolve_output_dir(&self.recording.directory, glib::UserDirectory::Videos)
    }
}

/// Expande `~` e cai no diretório XDG correspondente quando não configurado.
fn resolve_output_dir(configured: &Option<String>, fallback: glib::UserDirectory) -> PathBuf {
    if let Some(raw) = configured {
        return expand_tilde(raw);
    }
    glib::user_special_dir(fallback)
        .unwrap_or_else(|| PathBuf::from(home_dir()))
        .join(OUTPUT_SUBDIR)
}

fn expand_tilde(raw: &str) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => PathBuf::from(home_dir()).join(rest),
        None if raw == "~" => PathBuf::from(home_dir()),
        None => PathBuf::from(raw),
    }
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
}

pub(crate) fn user_config_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".config"))
}

/// Avisa se o arquivo de credenciais for legível/gravável por grupo ou outros.
fn warn_on_loose_permissions(path: &Path) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        tracing::warn!(
            path = %path.display(),
            mode = format!("{mode:o}"),
            "arquivo de credenciais acessível por outros usuários; corrija com `chmod 600 {}`",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// Testes
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [nvr]
        host = "192.168.77.30"
        username = "admin"
        password = "hunter2"

        [[cameras]]
        name = "Portão"
        channel = 1
    "#;

    fn parse(raw: &str) -> Result<Config> {
        let config: Config = toml::from_str(raw)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn aplica_valores_padrao() {
        let config = parse(MINIMAL).expect("config mínima deve ser válida");
        let nvr = config.all_nvrs()[0];
        assert_eq!(nvr.rtsp_port, 554);
        assert_eq!(nvr.url_template, DEFAULT_URL_TEMPLATE);
        assert_eq!(nvr.id(), "192.168.77.30", "id cai no host");
        assert_eq!(config.app.latency_ms, 200);
        assert_eq!(config.app.rtsp_protocols, "tcp");
        assert_eq!(config.app.grid_columns, None);
        assert!(config.app.hardware_decoding);
        assert!(!config.app.adaptive_stream);
        assert_eq!(config.app.substream_index, 1);
        assert!(config.app.wait_for_keyframe);
        assert_eq!(config.app.keyframe_timeout_secs, 90);
        assert_eq!(config.cameras[0].stream, 0);
        assert!(config.cameras[0].enabled);
        assert_eq!(config.recording.container, "mkv");
        assert!(!config.motion.enabled);
        assert!(config.notifications.enabled);
    }

    #[test]
    fn senha_nunca_aparece_em_claro() {
        let config = parse(MINIMAL).unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter2"), "senha vazou em Debug: {debug}");
        assert!(debug.contains(MASK));
        assert_eq!(format!("{}", config.all_nvrs()[0].password), MASK);
        assert_eq!(config.all_nvrs()[0].password.expose(), "hunter2");
    }

    #[test]
    fn campo_desconhecido_e_erro() {
        let raw = format!("{MINIMAL}\n[app]\nlatencia_ms = 300\n");
        let err = parse(&raw).unwrap_err().to_string();
        assert!(err.contains("latencia_ms"), "erro inesperado: {err}");
    }

    #[test]
    fn exige_ao_menos_uma_camera_habilitada() {
        let raw = format!("{MINIMAL}enabled = false\n");
        let err = parse(&raw).unwrap_err().to_string();
        assert!(err.contains("nenhuma câmera habilitada"), "erro: {err}");
    }

    #[test]
    fn rejeita_backoff_invertido() {
        let raw =
            format!("{MINIMAL}\n[app]\nreconnect_initial_secs = 30\nreconnect_max_secs = 5\n");
        let err = parse(&raw).unwrap_err().to_string();
        assert!(err.contains("reconnect_max_secs"), "erro: {err}");
    }

    #[test]
    fn rejeita_host_vazio() {
        let raw = MINIMAL.replace("192.168.77.30", "");
        let err = parse(&raw).unwrap_err().to_string();
        assert!(err.contains("nvr.host"), "erro: {err}");
    }

    #[test]
    fn rejeita_container_desconhecido() {
        let raw = format!("{MINIMAL}\n[recording]\ncontainer = \"avi\"\n");
        let err = parse(&raw).unwrap_err().to_string();
        assert!(err.contains("recording.container"), "erro: {err}");
    }

    #[test]
    fn rejeita_sensibilidade_fora_da_faixa() {
        let raw = format!("{MINIMAL}\n[motion]\nsensitivity = 1.5\n");
        let err = parse(&raw).unwrap_err().to_string();
        assert!(err.contains("motion.sensitivity"), "erro: {err}");
    }

    #[test]
    fn le_configuracao_completa() {
        let raw = r#"
            [nvr]
            host = "10.0.0.5"
            rtsp_port = 8554
            username = "operador"
            password = "s3nh@"
            url_template = "rtsp://{host}:{port}/ch{channel}"

            [app]
            latency_ms = 400
            grid_columns = 3
            hardware_decoding = false
            rtsp_protocols = "tcp+udp"
            stall_timeout_secs = 20
            wait_for_keyframe = false
            keyframe_timeout_secs = 45
            reconnect_initial_secs = 1
            reconnect_max_secs = 120
            adaptive_stream = true
            substream_index = 2

            [recording]
            directory = "~/gravacoes"
            segment_seconds = 60
            max_files = 10
            container = "mp4"

            [motion]
            enabled = true
            threshold = 40
            sensitivity = 0.1
            cooldown_secs = 30
            notify = true

            [notifications]
            enabled = false
            offline_after_attempts = 5
            tray = false

            [[cameras]]
            name = "Frente"
            channel = 1
            stream = 1

            [[cameras]]
            name = "Fundos"
            channel = 2
            enabled = false
        "#;
        let config = parse(raw).unwrap();
        assert_eq!(config.all_nvrs()[0].rtsp_port, 8554);
        assert_eq!(config.app.grid_columns, Some(3));
        assert!(!config.app.hardware_decoding);
        assert!(config.app.adaptive_stream);
        assert_eq!(config.app.substream_index, 2);
        assert!(!config.app.wait_for_keyframe);
        assert_eq!(config.app.keyframe_timeout_secs, 45);
        assert_eq!(config.cameras.len(), 2);
        assert_eq!(config.enabled_cameras().count(), 1);
        assert_eq!(config.cameras[0].stream, 1);
        assert_eq!(config.recording.max_files, 10);
        assert!(config.motion.notify);
        assert!(!config.notifications.tray);
        assert!(config.recording_dir().ends_with("gravacoes"));
    }

    // -- múltiplos NVRs -----------------------------------------------------

    const DOIS_NVRS: &str = r#"
        [nvr]
        id = "casa"
        host = "192.168.77.30"
        username = "admin"
        password = "a"

        [[nvrs]]
        id = "loja"
        host = "192.168.9.10"
        username = "admin"
        password = "b"

        [[cameras]]
        name = "Portão"
        channel = 1
        nvr = "casa"

        [[cameras]]
        name = "Caixa"
        channel = 1
        nvr = "loja"
    "#;

    #[test]
    fn resolve_cameras_de_varios_nvrs() {
        let config = parse(DOIS_NVRS).unwrap();
        assert_eq!(config.all_nvrs().len(), 2);
        assert_eq!(
            config.nvr_for(&config.cameras[0]).unwrap().host,
            "192.168.77.30"
        );
        assert_eq!(
            config.nvr_for(&config.cameras[1]).unwrap().host,
            "192.168.9.10"
        );
    }

    #[test]
    fn com_varios_nvrs_a_camera_precisa_dizer_qual() {
        let raw = DOIS_NVRS.replace("        nvr = \"casa\"\n", "");
        let err = parse(&raw).unwrap_err().to_string();
        assert!(err.contains("precisa de `nvr"), "erro: {err}");
    }

    #[test]
    fn rejeita_referencia_a_nvr_inexistente() {
        let raw = DOIS_NVRS.replace("nvr = \"loja\"", "nvr = \"fazenda\"");
        let err = parse(&raw).unwrap_err().to_string();
        assert!(err.contains("fazenda"), "erro: {err}");
    }

    #[test]
    fn rejeita_ids_duplicados() {
        let raw = DOIS_NVRS.replace("id = \"loja\"", "id = \"casa\"");
        let err = parse(&raw).unwrap_err().to_string();
        assert!(err.contains("mesmo id"), "erro: {err}");
    }

    #[test]
    fn expande_til_no_diretorio() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_tilde("~/x/y"), PathBuf::from(&home).join("x/y"));
        assert_eq!(expand_tilde("~"), PathBuf::from(&home));
        assert_eq!(expand_tilde("/abs/oluto"), PathBuf::from("/abs/oluto"));
    }
}

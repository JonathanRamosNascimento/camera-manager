//! Modelo de câmera, construção da URL RTSP e limpeza de segredos em texto.

use std::sync::Arc;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::config::{App, MASK};
use crate::store::{ChannelEntry, Device};

/// Caracteres que precisam ser escapados na seção `user:senha@` da URL.
/// Mantemos os "unreserved" da RFC 3986 (`-`, `.`, `_`, `~`) sem escape.
const USERINFO: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

// ---------------------------------------------------------------------------
// Template de URL
// ---------------------------------------------------------------------------

/// Dados de um gravador já preparados para renderizar URLs de qualquer
/// canal/stream. Compartilhado por `Arc` entre as câmeras do mesmo NVR.
#[derive(Debug)]
pub struct UrlTemplate {
    template: String,
    host: String,
    port: u16,
    user: String,
    user_enc: String,
    password: String,
    password_enc: String,
}

impl UrlTemplate {
    pub fn new(device: &Device) -> Self {
        let password = device.password.expose().to_string();
        Self {
            template: device.template().to_string(),
            host: device.host.clone(),
            port: device.port,
            user_enc: utf8_percent_encode(&device.username, USERINFO).to_string(),
            user: device.username.clone(),
            password_enc: utf8_percent_encode(&password, USERINFO).to_string(),
            password,
        }
    }

    /// URL com credenciais em claro. Só deve ir para o `rtspsrc`.
    pub fn render(&self, channel: u32, stream: u8) -> String {
        self.render_with(channel, stream, &self.password, &self.password_enc)
    }

    /// URL com a senha mascarada, para logs, UI e diagnóstico.
    ///
    /// A máscara entra igual nos dois slots: percent-encodá-la geraria um
    /// `%2A%2A%2A` ilegível sem ganho nenhum de segurança.
    pub fn render_masked(&self, channel: u32, stream: u8) -> String {
        self.render_with(channel, stream, MASK, MASK)
    }

    /// As duas formas da senha vêm de fora, de modo que a mesma função gere
    /// tanto a URL real quanto a mascarada — não há caminho em que a senha
    /// "escape" por um dos lados.
    fn render_with(&self, channel: u32, stream: u8, password: &str, password_enc: &str) -> String {
        self.template
            .replace("{host}", &self.host)
            .replace("{port}", &self.port.to_string())
            .replace("{channel}", &channel.to_string())
            .replace("{stream}", &stream.to_string())
            .replace("{user_enc}", &self.user_enc)
            .replace("{password_enc}", password_enc)
            .replace("{user}", &self.user)
            .replace("{password}", password)
    }
}

// ---------------------------------------------------------------------------
// Câmera
// ---------------------------------------------------------------------------

/// Qualidade de imagem escolhida para uma câmera no grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    /// Stream principal.
    High,
    /// Substream: menos resolução, CPU e banda.
    Low,
}

/// Uma câmera pronta para virar pipeline.
#[derive(Debug, Clone)]
pub struct Camera {
    pub id: usize,
    pub name: String,
    pub channel: u32,
    /// Stream de alta qualidade, usado no fullscreen.
    pub main_stream: u8,
    /// Stream usado no grid — igual ao principal, exceto com `adaptive_stream`.
    pub grid_stream: u8,
    /// Substream (`app.substream_index`), oferecido no seletor de qualidade.
    pub sub_stream: u8,
    /// Identificador do gravador ao qual a câmera pertence.
    pub nvr_id: String,
    pub host: String,
    pub port: u16,
    urls: Arc<UrlTemplate>,
}

impl Camera {
    /// URL RTSP para um índice de stream específico. Contém a senha.
    pub fn url_for(&self, stream: u8) -> String {
        self.urls.render(self.channel, stream)
    }

    /// Mesma URL, com a senha mascarada.
    pub fn masked_url_for(&self, stream: u8) -> String {
        self.urls.render_masked(self.channel, stream)
    }

    /// O gravador expõe um substream distinto do principal?
    pub fn has_substream(&self) -> bool {
        self.sub_stream != self.main_stream
    }

    /// Índice de stream que corresponde à qualidade pedida.
    pub fn stream_for(&self, quality: Quality) -> u8 {
        match quality {
            Quality::High => self.main_stream,
            Quality::Low => self.sub_stream,
        }
    }

    /// Qualidade que o stream do grid representa hoje.
    pub fn grid_quality(&self) -> Quality {
        if self.has_substream() && self.grid_stream == self.sub_stream {
            Quality::Low
        } else {
            Quality::High
        }
    }

    /// Chave estável (gravador + canal) para guardar preferências.
    pub fn pref_key(&self) -> String {
        format!("{}/{}", self.nvr_id, self.channel)
    }

    /// Rótulo curto usado em logs estruturados e nomes de elementos GStreamer.
    pub fn label(&self) -> String {
        format!("cam{}·ch{}", self.id, self.channel)
    }

    /// Nome seguro para arquivo, derivado do nome da câmera.
    pub fn slug(&self) -> String {
        let mut slug = String::with_capacity(self.name.len());
        let mut last_dash = true;
        for ch in self.name.chars() {
            let folded = fold_accent(ch);
            if folded.is_ascii_alphanumeric() {
                slug.push(folded.to_ascii_lowercase());
                last_dash = false;
            } else if !last_dash {
                slug.push('-');
                last_dash = true;
            }
        }
        let slug = slug.trim_matches('-').to_string();
        if slug.is_empty() {
            format!("ch{}", self.channel)
        } else {
            slug
        }
    }
}

/// Constrói as câmeras de um dispositivo, uma por canal cadastrado.
///
/// `first_id` é o id da primeira; as demais seguem em sequência. Todas
/// compartilham um único [`UrlTemplate`], então as credenciais existem uma vez
/// só na memória por dispositivo.
pub fn build_device(first_id: usize, device: &Device, app: &App) -> Vec<Camera> {
    let urls = Arc::new(UrlTemplate::new(device));
    device
        .channels
        .iter()
        .enumerate()
        .map(|(offset, entry)| build_one(first_id + offset, device, &urls, entry, app))
        .collect()
}

pub fn build_one(
    id: usize,
    device: &Device,
    urls: &Arc<UrlTemplate>,
    entry: &ChannelEntry,
    app: &App,
) -> Camera {
    // Com `adaptive_stream`, o grid roda no substream para poupar CPU/banda e
    // o fullscreen troca para o principal.
    let grid_stream = if app.adaptive_stream {
        app.substream_index
    } else {
        entry.stream
    };

    Camera {
        id,
        name: entry.name.clone(),
        channel: entry.channel,
        main_stream: entry.stream,
        grid_stream,
        sub_stream: app.substream_index,
        nvr_id: device.id.clone(),
        host: device.host.clone(),
        port: device.port,
        urls: Arc::clone(urls),
    }
}

/// Mapeia acentos latinos para ASCII, para nomes de arquivo legíveis.
fn fold_accent(ch: char) -> char {
    match ch {
        'á' | 'à' | 'â' | 'ã' | 'ä' | 'Á' | 'À' | 'Â' | 'Ã' | 'Ä' => 'a',
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' | 'Ó' | 'Ò' | 'Ô' | 'Õ' | 'Ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Ù' | 'Û' | 'Ü' => 'u',
        'ç' | 'Ç' => 'c',
        'ñ' | 'Ñ' => 'n',
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Redactor
// ---------------------------------------------------------------------------

/// Remove as senhas dos NVRs de qualquer texto vindo do GStreamer.
///
/// Mensagens de erro do `rtspsrc` costumam ecoar a `location` completa, que
/// inclui as credenciais. Como esse texto vai para o log e para a UI, ele passa
/// primeiro por aqui.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Formas em que as senhas podem aparecer: literal e percent-encoded.
    needles: Vec<String>,
}

impl Redactor {
    /// Constrói a partir das senhas de todos os dispositivos.
    pub fn new<'a>(devices: impl IntoIterator<Item = &'a Device>) -> Self {
        let mut redactor = Self::default();
        for device in devices {
            redactor.add_password(device.password.expose());
        }
        redactor
    }

    /// Passa a mascarar também esta senha (dispositivo cadastrado depois).
    pub fn add_password(&mut self, password: &str) {
        if password.is_empty() {
            return;
        }
        let encoded = utf8_percent_encode(password, USERINFO).to_string();
        if encoded != password {
            self.needles.push(encoded);
        }
        self.needles.push(password.to_string());
        // Substituir primeiro as formas mais longas evita que um prefixo comum
        // corte a variante maior pela metade.
        self.needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        self.needles.dedup();
    }

    pub fn apply(&self, text: &str) -> String {
        self.needles.iter().fold(text.to_string(), |acc, needle| {
            acc.replace(needle.as_str(), MASK)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Secret;

    fn device(user: &str, password: &str) -> Device {
        Device {
            id: "192.168.77.30:554".into(),
            name: "NVR".into(),
            host: "192.168.77.30".into(),
            port: 554,
            username: user.into(),
            password: Secret::new(password),
            url_template: None,
            channels: vec![ChannelEntry {
                channel: 1,
                name: "Portão".into(),
                stream: 0,
            }],
        }
    }

    fn app(raw: &str) -> App {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            #[serde(default)]
            app: App,
        }
        toml::from_str::<Wrapper>(raw)
            .expect("TOML de teste válido")
            .app
    }

    fn one(device: &Device, app: &App) -> Camera {
        build_device(0, device, app).remove(0)
    }

    #[test]
    fn monta_url_no_formato_do_nvr() {
        let camera = one(&device("admin", "1234"), &app(""));
        assert_eq!(
            camera.url_for(0),
            "rtsp://admin:1234@192.168.77.30:554/user=admin&password=1234&channel=1&stream=0.sdp"
        );
    }

    #[test]
    fn url_por_stream_muda_apenas_o_campo_stream() {
        let camera = one(&device("admin", "1234"), &app(""));
        assert!(camera.url_for(1).ends_with("&channel=1&stream=1.sdp"));
        assert!(camera.url_for(0).ends_with("&channel=1&stream=0.sdp"));
    }

    #[test]
    fn url_mascarada_esconde_a_senha() {
        let camera = one(&device("admin", "s3nh@!"), &app(""));
        let masked = camera.masked_url_for(0);
        assert!(!masked.contains("s3nh@!"));
        assert!(!masked.contains("s3nh%40%21"));
        assert_eq!(
            masked,
            "rtsp://admin:***@192.168.77.30:554/user=admin&password=***&channel=1&stream=0.sdp"
        );
    }

    #[test]
    fn escapa_credenciais_apenas_no_userinfo() {
        // O `@` quebraria o parsing da URL antes do host, mas o NVR espera o
        // valor literal no par `password=` do path.
        let camera = one(&device("adm in", "a@b/c"), &app(""));
        let url = camera.url_for(0);
        assert!(url.starts_with("rtsp://adm%20in:a%40b%2Fc@192.168.77.30:554/"));
        assert!(url.contains("user=adm in&password=a@b/c&"));
    }

    #[test]
    fn respeita_template_customizado() {
        let mut d = device("admin", "1234");
        d.url_template = Some("rtsp://{host}:{port}/live/ch{channel}?q={stream}".into());
        let camera = one(&d, &app(""));
        assert_eq!(camera.url_for(0), "rtsp://192.168.77.30:554/live/ch1?q=0");
        assert_eq!(camera.url_for(1), "rtsp://192.168.77.30:554/live/ch1?q=1");
    }

    #[test]
    fn adaptive_stream_usa_substream_no_grid() {
        let d = device("admin", "1234");
        let camera = one(&d, &app("[app]\nadaptive_stream = true\n"));
        assert_eq!(camera.main_stream, 0, "fullscreen continua no principal");
        assert_eq!(camera.grid_stream, 1, "grid cai para o substream");
        assert_eq!(camera.grid_quality(), Quality::Low);

        let camera = one(&d, &app(""));
        assert_eq!(camera.grid_stream, 0, "sem adaptive, grid = principal");
        assert_eq!(camera.grid_quality(), Quality::High);
    }

    #[test]
    fn um_dispositivo_gera_uma_camera_por_canal_com_ids_em_sequencia() {
        let mut d = device("admin", "1234");
        d.channels.push(ChannelEntry {
            channel: 3,
            name: "Quintal".into(),
            stream: 0,
        });
        let cameras = build_device(5, &d, &app(""));
        assert_eq!(cameras.iter().map(|c| c.id).collect::<Vec<_>>(), vec![5, 6]);
        assert_eq!(cameras[1].channel, 3);
        assert_eq!(cameras[1].pref_key(), "192.168.77.30:554/3");
    }

    #[test]
    fn label_e_slug() {
        let mut d = device("admin", "1234");
        d.channels[0].name = "Câmera dos Fundos".into();
        let camera = one(&d, &app(""));
        assert_eq!(camera.label(), "cam0·ch1");
        assert_eq!(camera.slug(), "camera-dos-fundos");
    }

    #[test]
    fn slug_cai_no_canal_quando_o_nome_nao_tem_ascii() {
        let mut d = device("admin", "1234");
        d.channels[0].name = "日本".into();
        assert_eq!(one(&d, &app("")).slug(), "ch1");
    }

    #[test]
    fn redactor_limpa_senha_literal_e_codificada() {
        let redactor = Redactor::new([&device("admin", "a@b")]);
        let sujo = "Could not open rtsp://admin:a%40b@10.0.0.1/x (user=admin&password=a@b)";
        let limpo = redactor.apply(sujo);
        assert!(!limpo.contains("a@b"), "senha literal vazou: {limpo}");
        assert!(!limpo.contains("a%40b"), "senha codificada vazou: {limpo}");
        assert_eq!(limpo.matches(MASK).count(), 2);
    }

    #[test]
    fn redactor_cobre_todos_os_dispositivos_e_os_adicionados_depois() {
        let mut redactor = Redactor::new([&device("u", "senha-casa")]);
        redactor.add_password("senha-loja");
        let limpo = redactor.apply("senha-casa e senha-loja");
        assert!(!limpo.contains("senha-casa") && !limpo.contains("senha-loja"));
    }
}

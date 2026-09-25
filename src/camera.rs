//! Modelo de câmera, construção da URL RTSP e limpeza de segredos em texto.

use std::sync::Arc;

use anyhow::Result;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::config::{CameraEntry, Config, MASK, Nvr};

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
    pub fn new(nvr: &Nvr) -> Self {
        let password = nvr.password.expose().to_string();
        Self {
            template: nvr.url_template.clone(),
            host: nvr.host.clone(),
            port: nvr.rtsp_port,
            user_enc: utf8_percent_encode(&nvr.username, USERINFO).to_string(),
            user: nvr.username.clone(),
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

/// Constrói uma câmera para cada entrada habilitada da configuração.
///
/// Câmeras do mesmo gravador compartilham um único [`UrlTemplate`], de modo que
/// as credenciais existam uma vez só na memória por NVR.
pub fn build_all(config: &Config) -> Result<Vec<Camera>> {
    let templates: Vec<(String, Arc<UrlTemplate>)> = config
        .all_nvrs()
        .into_iter()
        .map(|nvr| (nvr.id().to_string(), Arc::new(UrlTemplate::new(nvr))))
        .collect();

    config
        .enabled_cameras()
        .enumerate()
        .map(|(id, entry)| build_one(config, &templates, id, entry))
        .collect()
}

fn build_one(
    config: &Config,
    templates: &[(String, Arc<UrlTemplate>)],
    id: usize,
    entry: &CameraEntry,
) -> Result<Camera> {
    let nvr = config.nvr_for(entry)?;
    let nvr_id = nvr.id().to_string();
    let urls = templates
        .iter()
        .find(|(candidate, _)| *candidate == nvr_id)
        .map(|(_, template)| Arc::clone(template))
        .expect("todo NVR resolvido tem um template");

    // Com `adaptive_stream`, o grid roda no substream para poupar CPU/banda e
    // o fullscreen troca para o principal.
    let grid_stream = if config.app.adaptive_stream {
        config.app.substream_index
    } else {
        entry.stream
    };

    Ok(Camera {
        id,
        name: entry.name.clone(),
        channel: entry.channel,
        main_stream: entry.stream,
        grid_stream,
        sub_stream: config.app.substream_index,
        nvr_id,
        host: nvr.host.clone(),
        port: nvr.rtsp_port,
        urls,
    })
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
    /// Constrói a partir de todas as senhas configuradas.
    pub fn new(config: &Config) -> Self {
        let mut needles = Vec::new();
        for nvr in config.all_nvrs() {
            let password = nvr.password.expose();
            if password.is_empty() {
                continue;
            }
            let encoded = utf8_percent_encode(password, USERINFO).to_string();
            if encoded != password {
                needles.push(encoded);
            }
            needles.push(password.to_string());
        }
        // Substituir primeiro as formas mais longas evita que um prefixo comum
        // corte a variante maior pela metade.
        needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        needles.dedup();
        Self { needles }
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

    fn config(raw: &str) -> Config {
        toml::from_str(raw).expect("TOML de teste válido")
    }

    const BASE: &str = r#"
        [nvr]
        host = "192.168.77.30"
        username = "admin"
        password = "1234"

        [[cameras]]
        name = "Portão"
        channel = 1
    "#;

    #[test]
    fn monta_url_no_formato_do_nvr() {
        let cameras = build_all(&config(BASE)).unwrap();
        assert_eq!(
            cameras[0].url_for(0),
            "rtsp://admin:1234@192.168.77.30:554/user=admin&password=1234&channel=1&stream=0.sdp"
        );
    }

    #[test]
    fn url_por_stream_muda_apenas_o_campo_stream() {
        let cameras = build_all(&config(BASE)).unwrap();
        assert!(cameras[0].url_for(1).ends_with("&channel=1&stream=1.sdp"));
        assert!(cameras[0].url_for(0).ends_with("&channel=1&stream=0.sdp"));
    }

    #[test]
    fn url_mascarada_esconde_a_senha() {
        let raw = BASE.replace("\"1234\"", "\"s3nh@!\"");
        let cameras = build_all(&config(&raw)).unwrap();
        let masked = cameras[0].masked_url_for(0);
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
        let raw = BASE
            .replace("\"admin\"", "\"adm in\"")
            .replace("\"1234\"", "\"a@b/c\"");
        let cameras = build_all(&config(&raw)).unwrap();
        let url = cameras[0].url_for(0);
        assert!(url.starts_with("rtsp://adm%20in:a%40b%2Fc@192.168.77.30:554/"));
        assert!(url.contains("user=adm in&password=a@b/c&"));
    }

    #[test]
    fn respeita_template_customizado() {
        let raw = r#"
            [nvr]
            host = "192.168.77.30"
            username = "admin"
            password = "1234"
            url_template = "rtsp://{host}:{port}/live/ch{channel}?q={stream}"

            [[cameras]]
            name = "Portão"
            channel = 1
        "#;
        let cameras = build_all(&config(raw)).unwrap();
        assert_eq!(
            cameras[0].url_for(0),
            "rtsp://192.168.77.30:554/live/ch1?q=0"
        );
        assert_eq!(
            cameras[0].url_for(1),
            "rtsp://192.168.77.30:554/live/ch1?q=1"
        );
    }

    #[test]
    fn adaptive_stream_usa_substream_no_grid() {
        let raw = format!("{BASE}\n[app]\nadaptive_stream = true\n");
        let cameras = build_all(&config(&raw)).unwrap();
        assert_eq!(
            cameras[0].main_stream, 0,
            "fullscreen continua no principal"
        );
        assert_eq!(cameras[0].grid_stream, 1, "grid cai para o substream");

        let cameras = build_all(&config(BASE)).unwrap();
        assert_eq!(cameras[0].grid_stream, 0, "sem adaptive, grid = principal");
    }

    #[test]
    fn label_e_slug() {
        let raw = BASE.replace("\"Portão\"", "\"Câmera dos Fundos\"");
        let cameras = build_all(&config(&raw)).unwrap();
        assert_eq!(cameras[0].label(), "cam0·ch1");
        assert_eq!(cameras[0].slug(), "camera-dos-fundos");
    }

    #[test]
    fn slug_cai_no_canal_quando_o_nome_nao_tem_ascii() {
        let raw = BASE.replace("\"Portão\"", "\"日本\"");
        let cameras = build_all(&config(&raw)).unwrap();
        assert_eq!(cameras[0].slug(), "ch1");
    }

    #[test]
    fn cameras_de_nvrs_diferentes_apontam_para_hosts_diferentes() {
        let raw = r#"
            [nvr]
            id = "casa"
            host = "192.168.77.30"
            username = "admin"
            password = "a"

            [[nvrs]]
            id = "loja"
            host = "10.0.0.9"
            rtsp_port = 8554
            username = "op"
            password = "b"

            [[cameras]]
            name = "Portão"
            channel = 1
            nvr = "casa"

            [[cameras]]
            name = "Caixa"
            channel = 2
            nvr = "loja"
        "#;
        let cameras = build_all(&config(raw)).unwrap();
        assert_eq!(cameras[0].host, "192.168.77.30");
        assert_eq!(cameras[0].nvr_id, "casa");
        assert_eq!(cameras[1].host, "10.0.0.9");
        assert_eq!(cameras[1].port, 8554);
        assert!(cameras[1].url_for(0).contains("op:b@10.0.0.9:8554"));
    }

    #[test]
    fn redactor_limpa_senha_literal_e_codificada() {
        let raw = BASE.replace("\"1234\"", "\"a@b\"");
        let redactor = Redactor::new(&config(&raw));
        let sujo = "Could not open rtsp://admin:a%40b@10.0.0.1/x (user=admin&password=a@b)";
        let limpo = redactor.apply(sujo);
        assert!(!limpo.contains("a@b"), "senha literal vazou: {limpo}");
        assert!(!limpo.contains("a%40b"), "senha codificada vazou: {limpo}");
        assert_eq!(limpo.matches(MASK).count(), 2);
    }

    #[test]
    fn redactor_cobre_todos_os_nvrs() {
        let raw = r#"
            [nvr]
            id = "casa"
            host = "h1"
            username = "u"
            password = "senha-casa"

            [[nvrs]]
            id = "loja"
            host = "h2"
            username = "u"
            password = "senha-loja"

            [[cameras]]
            name = "A"
            channel = 1
            nvr = "casa"
        "#;
        let redactor = Redactor::new(&config(raw));
        let limpo = redactor.apply("senha-casa e senha-loja");
        assert_eq!(limpo, "*** e ***");
    }
}

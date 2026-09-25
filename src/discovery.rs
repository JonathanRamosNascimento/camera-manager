//! Descoberta de dispositivos na rede e teste de canais.
//!
//! Duas fontes se complementam na varredura:
//! - **porta RTSP aberta** (TCP 554) em cada IP da sub-rede: é o que NVRs
//!   iCSee/XMEye e a maioria das câmeras IP expõem;
//! - **ONVIF / WS-Discovery** (multicast UDP 3702): câmeras padrão respondem
//!   com nome e modelo. NVRs iCSee normalmente não respondem, por isso a
//!   varredura de portas é a base.
//!
//! O teste de canais abre de verdade o RTSP e só considera um canal "vivo" se
//! chegar dado de vídeo. Isso é necessário porque muitos NVRs aceitam a sessão
//! RTSP até para canais que não existem (verificado no NVR de referência:
//! canal 4 e 9 abrem `SETUP` normalmente), então "conectou" não prova nada.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use gst::prelude::*;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::Semaphore;

use crate::camera::{Redactor, UrlTemplate};
use crate::store::Device;

/// Conexões TCP simultâneas durante a varredura.
const SCAN_CONCURRENCY: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(700);
const ONVIF_LISTEN: Duration = Duration::from_millis(2500);
/// Canais testados em paralelo: NVRs de entrada limitam conexões simultâneas.
const PROBE_BATCH: usize = 4;
/// Quanto esperar por vídeo em cada canal.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

// ---------------------------------------------------------------------------
// Varredura de rede
// ---------------------------------------------------------------------------

/// Um dispositivo encontrado na rede.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub ip: Ipv4Addr,
    /// A porta RTSP respondeu.
    pub rtsp_open: bool,
    /// Nome/modelo anunciado via ONVIF, quando houve resposta.
    pub onvif: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ScanEvent {
    /// `feitos` de `total` IPs verificados.
    Progress {
        done: usize,
        total: usize,
    },
    /// Achou (ou atualizou) um dispositivo.
    Found(Found),
    Finished,
}

/// Os 3 primeiros octetos da sub-rede (`/24`) da máquina, se der para descobrir.
///
/// Abre um socket UDP "conectado" a um endereço público — nenhum pacote é
/// enviado — só para o kernel dizer qual IP local sairia pela rota padrão.
pub fn local_prefix() -> Option<[u8; 3]> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() => {
            let [a, b, c, _] = ip.octets();
            Some([a, b, c])
        }
        _ => None,
    }
}

/// Interpreta `192.168.77`, `192.168.77.0`, `192.168.77.0/24` ou um IP completo.
pub fn parse_prefix(text: &str) -> Result<[u8; 3]> {
    let base = text.trim().split('/').next().unwrap_or_default();
    let octets: Vec<u8> = base
        .split('.')
        .map(|part| part.trim().parse::<u8>())
        .collect::<Result<_, _>>()
        .map_err(|_| anyhow::anyhow!("sub-rede inválida: \"{text}\" (ex.: 192.168.1.0/24)"))?;
    match octets.as_slice() {
        [a, b, c] | [a, b, c, _] => Ok([*a, *b, *c]),
        _ => bail!("sub-rede inválida: \"{text}\" (ex.: 192.168.1.0/24)"),
    }
}

/// Varre `prefix.1 … prefix.254` na porta RTSP e escuta respostas ONVIF.
///
/// Os resultados chegam por `events`; a função só retorna ao terminar. Se o
/// receptor for fechado (janela de scan fechada), a varredura para sozinha.
pub async fn scan(prefix: [u8; 3], port: u16, events: async_channel::Sender<ScanEvent>) {
    let total = 254usize;
    let semaphore = Arc::new(Semaphore::new(SCAN_CONCURRENCY));
    let (result_tx, result_rx) = async_channel::unbounded::<(Ipv4Addr, bool)>();

    for host in 1..=254u8 {
        let ip = Ipv4Addr::new(prefix[0], prefix[1], prefix[2], host);
        let semaphore = Arc::clone(&semaphore);
        let result_tx = result_tx.clone();
        tokio::spawn(async move {
            let Ok(_permit) = semaphore.acquire().await else {
                return;
            };
            let open = tcp_open(SocketAddr::from((ip, port))).await;
            let _ = result_tx.send((ip, open)).await;
        });
    }
    drop(result_tx);

    // ONVIF em paralelo com a varredura de portas.
    let onvif_task = tokio::spawn(ws_discovery());

    let mut found: BTreeMap<Ipv4Addr, Found> = BTreeMap::new();
    let mut done = 0usize;
    while let Ok((ip, open)) = result_rx.recv().await {
        done += 1;
        if open {
            let entry = Found {
                ip,
                rtsp_open: true,
                onvif: None,
            };
            found.insert(ip, entry.clone());
            if events.send(ScanEvent::Found(entry)).await.is_err() {
                return;
            }
        }
        if (done.is_multiple_of(8) || done == total)
            && events
                .send(ScanEvent::Progress { done, total })
                .await
                .is_err()
        {
            return;
        }
    }

    let onvif = onvif_task.await.unwrap_or_default();
    for (ip, name) in onvif {
        let entry = match found.get(&ip) {
            Some(existing) => Found {
                onvif: Some(name),
                ..existing.clone()
            },
            None => Found {
                ip,
                // Fora da varredura (ou sem 554): confirma a porta RTSP.
                rtsp_open: tcp_open(SocketAddr::from((ip, port))).await,
                onvif: Some(name),
            },
        };
        found.insert(ip, entry.clone());
        if events.send(ScanEvent::Found(entry)).await.is_err() {
            return;
        }
    }
    let _ = events.send(ScanEvent::Finished).await;
}

async fn tcp_open(addr: SocketAddr) -> bool {
    matches!(
        tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await,
        Ok(Ok(_))
    )
}

/// Envia um `Probe` WS-Discovery e devolve `ip → nome` de quem respondeu.
async fn ws_discovery() -> Vec<(Ipv4Addr, String)> {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else {
        return Vec::new();
    };
    let target = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 3702);
    let _ = socket.set_multicast_ttl_v4(2);
    if socket
        .send_to(onvif_probe().as_bytes(), target)
        .await
        .is_err()
    {
        return Vec::new();
    }

    let mut answers = BTreeMap::new();
    let deadline = Instant::now() + ONVIF_LISTEN;
    let mut buffer = vec![0u8; 16 * 1024];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match tokio::time::timeout(left, socket.recv_from(&mut buffer)).await {
            Ok(Ok((len, SocketAddr::V4(from)))) => {
                let text = String::from_utf8_lossy(&buffer[..len]);
                answers.insert(*from.ip(), onvif_name(&text));
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    answers.into_iter().collect()
}

fn onvif_probe() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let id = format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (nanos >> 64) as u32,
        (nanos >> 48) as u16,
        (nanos >> 32) as u16,
        (nanos >> 16) as u16,
        (nanos as u64) & 0xffff_ffff_ffff
    );
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
<e:Header><w:MessageID>uuid:{id}</w:MessageID><w:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To><w:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action></e:Header>
<e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body>
</e:Envelope>"#
    )
}

/// Extrai um nome legível dos `Scopes` de uma resposta WS-Discovery.
///
/// Os scopes ONVIF vêm como `onvif://www.onvif.org/name/Camera%20X`,
/// `.../hardware/Modelo`. Preferimos `name`, depois `hardware`.
fn onvif_name(response: &str) -> String {
    let scope = |key: &str| {
        response
            .split_whitespace()
            .flat_map(|word| word.split(['<', '>']))
            .find_map(|word| {
                word.split_once(&format!("onvif.org/{key}/"))
                    .map(|(_, v)| v)
            })
            .map(|value| value.replace("%20", " "))
            .filter(|value| !value.is_empty())
    };
    scope("name")
        .or_else(|| scope("hardware"))
        .unwrap_or_else(|| "câmera ONVIF".to_string())
}

// ---------------------------------------------------------------------------
// Teste de canais
// ---------------------------------------------------------------------------

/// Resultado do teste de um canal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// Chegou vídeo; guarda o codec (`H264`, `H265`…).
    Video(String),
    /// A sessão RTSP abriu mas nenhum dado chegou: canal sem câmera, câmera
    /// offline ou canal inexistente (o NVR não diferencia).
    NoData,
    Unauthorized,
    Error(String),
}

impl ProbeResult {
    pub fn is_video(&self) -> bool {
        matches!(self, Self::Video(_))
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Video(codec) => format!("vídeo ({codec})"),
            Self::NoData => "sem imagem (câmera offline ou canal vazio)".to_string(),
            Self::Unauthorized => "usuário/senha recusados".to_string(),
            Self::Error(reason) => format!("erro: {reason}"),
        }
    }
}

/// Testa `channels` em lotes de [`PROBE_BATCH`], avisando cada resultado assim
/// que sai. Termina ao esgotar os canais ou quando o receptor é fechado.
pub async fn probe_channels(
    device: Device,
    channels: Vec<u32>,
    results: async_channel::Sender<(u32, ProbeResult)>,
) {
    let urls = Arc::new(UrlTemplate::new(&device));
    let redactor = Redactor::new([&device]);

    for batch in channels.chunks(PROBE_BATCH) {
        let mut tasks = Vec::new();
        for &channel in batch {
            let url = urls.render(channel, 0);
            let redactor = redactor.clone();
            tasks.push((
                channel,
                tokio::task::spawn_blocking(move || probe_url(&url, &redactor, PROBE_TIMEOUT)),
            ));
        }
        for (channel, task) in tasks {
            let result = task
                .await
                .unwrap_or_else(|err| ProbeResult::Error(err.to_string()));
            if results.send((channel, result)).await.is_err() {
                return;
            }
        }
    }
}

/// Abre o RTSP e espera dado de vídeo, até `timeout`. Bloqueante.
fn probe_url(url: &str, redactor: &Redactor, timeout: Duration) -> ProbeResult {
    let pipeline = gst::Pipeline::new();
    let build = || -> Result<(gst::Element, gst::Element)> {
        let src = gst::ElementFactory::make("rtspsrc")
            .property("location", url)
            .property_from_str("protocols", "tcp")
            .property("timeout", 5_000_000u64)
            .property("tcp-timeout", 5_000_000u64)
            .build()?;
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()?;
        Ok((src, sink))
    };
    let (src, sink) = match build() {
        Ok(pair) => pair,
        Err(err) => return ProbeResult::Error(redactor.apply(&err.to_string())),
    };
    if pipeline.add_many([&src, &sink]).is_err() {
        return ProbeResult::Error("falha ao montar o teste".into());
    }

    let codec: Arc<Mutex<Option<String>>> = Arc::default();
    let got_data = Arc::new(AtomicBool::new(false));

    {
        let sink = sink.downgrade();
        let codec = Arc::clone(&codec);
        src.connect_pad_added(move |_, pad| {
            let Some(sink) = sink.upgrade() else { return };
            let Some(caps) = pad.current_caps() else {
                return;
            };
            let Some(structure) = caps.structure(0) else {
                return;
            };
            if structure.get::<String>("media").as_deref() != Ok("video") {
                return;
            }
            if let Ok(name) = structure.get::<String>("encoding-name") {
                *codec.lock().unwrap() = Some(name);
            }
            if let Some(sink_pad) = sink.static_pad("sink")
                && !sink_pad.is_linked()
            {
                let _ = pad.link(&sink_pad);
            }
        });
    }
    if let Some(pad) = sink.static_pad("sink") {
        let got_data = Arc::clone(&got_data);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            got_data.store(true, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });
    }

    let outcome = run_probe(&pipeline, &got_data, timeout, redactor);
    let _ = pipeline.set_state(gst::State::Null);

    match outcome {
        Some(failure) => failure,
        None if got_data.load(Ordering::Relaxed) => {
            let name = codec.lock().unwrap().clone().unwrap_or_else(|| "?".into());
            ProbeResult::Video(name)
        }
        None => ProbeResult::NoData,
    }
}

/// Roda a pipeline até chegar dado, dar erro ou estourar o prazo.
/// `Some` = falha; `None` = terminou sem erro (chegou dado ou esgotou o prazo).
fn run_probe(
    pipeline: &gst::Pipeline,
    got_data: &AtomicBool,
    timeout: Duration,
    redactor: &Redactor,
) -> Option<ProbeResult> {
    if pipeline.set_state(gst::State::Playing).is_err() {
        return Some(ProbeResult::Error("não consegui iniciar o teste".into()));
    }
    let bus = pipeline.bus()?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && !got_data.load(Ordering::Relaxed) {
        let Some(message) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(100),
            &[gst::MessageType::Error],
        ) else {
            continue;
        };
        if let gst::MessageView::Error(err) = message.view() {
            let text = format!(
                "{} {}",
                err.error(),
                err.debug().map(|d| d.to_string()).unwrap_or_default()
            );
            return Some(if text.contains("401") || text.contains("Unauthorized") {
                ProbeResult::Unauthorized
            } else {
                ProbeResult::Error(redactor.apply(err.error().message()))
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixo_aceita_varias_formas() {
        assert_eq!(parse_prefix("192.168.77.0/24").unwrap(), [192, 168, 77]);
        assert_eq!(parse_prefix("192.168.77").unwrap(), [192, 168, 77]);
        assert_eq!(parse_prefix(" 10.0.5.30 ").unwrap(), [10, 0, 5]);
        assert!(parse_prefix("192.168").is_err());
        assert!(parse_prefix("abc").is_err());
        assert!(parse_prefix("300.1.1.0/24").is_err());
    }

    #[test]
    fn nome_onvif_vem_dos_scopes() {
        let xml = "<d:Scopes>onvif://www.onvif.org/type/video_encoder \
                   onvif://www.onvif.org/name/Camera%20Sala \
                   onvif://www.onvif.org/hardware/IPC-123</d:Scopes>";
        assert_eq!(onvif_name(xml), "Camera Sala");
        let so_hardware = "<d:Scopes>onvif://www.onvif.org/hardware/IPC-123</d:Scopes>";
        assert_eq!(onvif_name(so_hardware), "IPC-123");
        assert_eq!(onvif_name("<x/>"), "câmera ONVIF");
    }

    #[test]
    fn probe_onvif_tem_o_tipo_certo() {
        let probe = onvif_probe();
        assert!(probe.contains("NetworkVideoTransmitter"));
        assert!(probe.contains("uuid:"));
    }

    #[tokio::test]
    async fn varredura_acha_porta_aberta_no_loopback() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { while listener.accept().await.is_ok() {} });

        let (tx, rx) = async_channel::unbounded();
        scan([127, 0, 0], port, tx).await;

        let mut ips = Vec::new();
        let mut finished = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                ScanEvent::Found(found) if found.rtsp_open => ips.push(found.ip),
                ScanEvent::Finished => finished = true,
                _ => {}
            }
        }
        assert!(finished);
        assert_eq!(ips, vec![Ipv4Addr::new(127, 0, 0, 1)]);
    }

    /// Teste manual contra um NVR de verdade:
    /// `NVR_TEST_HOST=… NVR_TEST_USER=… NVR_TEST_PASS=… cargo test -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "precisa de um NVR real"]
    async fn detecta_canais_no_nvr_real() {
        use crate::config::Secret;
        let (Ok(host), Ok(user), Ok(pass)) = (
            std::env::var("NVR_TEST_HOST"),
            std::env::var("NVR_TEST_USER"),
            std::env::var("NVR_TEST_PASS"),
        ) else {
            return;
        };
        gst::init().unwrap();
        let device = Device {
            id: Device::make_id(&host, 554),
            name: "teste".into(),
            host,
            port: 554,
            username: user,
            password: Secret::new(pass),
            url_template: None,
            channels: Vec::new(),
        };
        let (tx, rx) = async_channel::unbounded();
        probe_channels(device, (1..=6).collect(), tx).await;
        while let Ok((channel, result)) = rx.try_recv() {
            println!("canal {channel}: {}", result.describe());
        }
    }

    #[test]
    fn resultado_descreve_para_a_interface() {
        assert!(ProbeResult::Video("H265".into()).is_video());
        assert!(!ProbeResult::NoData.is_video());
        assert!(
            ProbeResult::Video("H264".into())
                .describe()
                .contains("H264")
        );
    }
}

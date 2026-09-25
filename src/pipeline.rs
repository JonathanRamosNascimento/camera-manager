//! Construção e instrumentação das pipelines GStreamer (uma por câmera).
//!
//! Topologia:
//! ```text
//!                        ┌─ queue ─▶ decodebin ─▶ tee_raw ─┬─ queue ─▶ gtk4paintablesink
//!  rtspsrc ─▶ tee_rtp ───┤                                 └─ queue ─▶ videoconvert ─▶ GRAY8 80×45 ─▶ fakesink   (movimento, opcional)
//!                        └─ queue ─▶ parsebin ─▶ splitmuxsink              (gravação, ramo dinâmico)
//! ```
//!
//! O `tee_rtp` deriva o stream **codificado**: gravar a partir dali evita
//! recodificar e mantém uma única conexão RTSP com o NVR.
//!
//! `rtspsrc` e `decodebin` expõem pads dinamicamente, então a ligação é feita em
//! callbacks. Os handlers são idempotentes: ao reconectar, a pipeline volta a
//! `NULL`, os pads dinâmicos somem e são religados sem recriar o sink — assim o
//! `GdkPaintable` entregue à UI continua válido durante toda a vida do app.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gst::prelude::*;
use gtk::gdk;

use crate::camera::Camera;
use crate::motion::MotionDetector;

/// Origem monotônica compartilhada, para guardar instantes em `AtomicU64`.
static START: LazyLock<Instant> = LazyLock::new(Instant::now);

fn now_ms() -> u64 {
    START.elapsed().as_millis() as u64
}

fn elapsed_since(mark: &AtomicU64) -> Duration {
    Duration::from_millis(now_ms().saturating_sub(mark.load(Ordering::Relaxed)))
}

/// Resolução do ramo de análise de movimento. Pequena de propósito: o diff é
/// por pixel e 80×45 já separa "alguém passou" de ruído de sensor.
const MOTION_WIDTH: i32 = 80;
const MOTION_HEIGHT: i32 = 45;
/// Taxa de amostragem da análise de movimento, em quadros por segundo.
const MOTION_FPS: i32 = 5;

// ---------------------------------------------------------------------------
// Estatísticas por stream
// ---------------------------------------------------------------------------

/// Contadores alimentados por pad probes.
///
/// Lidos tanto pelo watchdog (thread do tokio) quanto pela UI (thread do GTK),
/// por isso tudo é atômico e o struct é compartilhado via `Arc`.
#[derive(Debug, Default)]
pub struct StreamStats {
    frames: AtomicU64,
    /// Frames desta tentativa de conexão. Zera a cada `note_attempt`, o que
    /// impede que a sessão anterior faça a nova parecer "conectada" antes da
    /// hora (e com a resolução antiga).
    session_frames: AtomicU64,
    /// Bytes de RTP recebidos — base para o indicador de bitrate de rede.
    bytes: AtomicU64,
    /// Último quadro decodificado, em ms desde [`START`].
    last_frame_ms: AtomicU64,
    /// Último byte recebido do NVR, em ms desde [`START`].
    ///
    /// Separado de `last_frame_ms` de propósito: com `wait_for_keyframe`, o
    /// stream pode estar chegando normalmente e ainda assim não produzir
    /// quadro nenhum por dezenas de segundos. Confundir os dois faz o watchdog
    /// reiniciar a pipeline em loop, sem nunca alcançar o keyframe.
    last_data_ms: AtomicU64,
    /// Quantas vezes o detector de movimento disparou.
    motion_events: AtomicU64,
    last_motion_ms: AtomicU64,
    resolution: Mutex<Option<(i32, i32)>>,
}

impl StreamStats {
    /// Registra a chegada de um buffer no sink.
    fn note_frame(&self) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.session_frames.fetch_add(1, Ordering::Relaxed);
        self.last_frame_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn note_bytes(&self, count: u64) {
        self.bytes.fetch_add(count, Ordering::Relaxed);
        self.last_data_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn note_motion(&self) {
        self.motion_events.fetch_add(1, Ordering::Relaxed);
        self.last_motion_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// Marca uma tentativa de (re)conexão, zerando o relógio do watchdog para
    /// que o período de handshake do RTSP não seja confundido com travamento.
    /// Seguro chamar da thread do supervisor: a pipeline está em `NULL`, então
    /// nenhum probe está escrevendo nestes contadores.
    pub fn note_attempt(&self) {
        let now = now_ms();
        self.last_frame_ms.store(now, Ordering::Relaxed);
        self.last_data_ms.store(now, Ordering::Relaxed);
        self.session_frames.store(0, Ordering::Relaxed);
        *self.resolution.lock().unwrap() = None;
    }

    /// Total de frames desde o início do processo (não zera na reconexão).
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Frames recebidos desde a última tentativa de conexão.
    pub fn session_frames(&self) -> u64 {
        self.session_frames.load(Ordering::Relaxed)
    }

    /// Total de bytes RTP recebidos desde o início do processo.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Quantas detecções de movimento aconteceram até agora.
    pub fn motion_events(&self) -> u64 {
        self.motion_events.load(Ordering::Relaxed)
    }

    /// Houve movimento nos últimos `within`?
    ///
    /// Quem responde "nunca houve" é o contador, não o timestamp: `now_ms()`
    /// vale 0 no primeiro milissegundo de vida do processo, então 0 não serve
    /// de sentinela.
    pub fn motion_recent(&self, within: Duration) -> bool {
        if self.motion_events.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let last = self.last_motion_ms.load(Ordering::Relaxed);
        now_ms().saturating_sub(last) < within.as_millis() as u64
    }

    /// Tempo desde o último quadro decodificado (ou desde a tentativa).
    pub fn idle_for(&self) -> Duration {
        elapsed_since(&self.last_frame_ms)
    }

    /// Tempo desde o último byte recebido do NVR (ou desde a tentativa).
    pub fn data_idle_for(&self) -> Duration {
        elapsed_since(&self.last_data_ms)
    }

    /// Resolução negociada, quando já houve evento de caps.
    pub fn resolution(&self) -> Option<(i32, i32)> {
        *self.resolution.lock().unwrap()
    }

    fn set_resolution(&self, width: i32, height: i32) {
        *self.resolution.lock().unwrap() = Some((width, height));
    }
}

// ---------------------------------------------------------------------------
// Opções e resultado da construção
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PipelineOptions {
    pub latency_ms: u32,
    /// Flags de transporte do `rtspsrc` (`tcp`, `udp`, `tcp+udp`).
    pub protocols: String,
    /// Detector de movimento; `None` desliga o ramo de análise.
    pub motion: Option<Arc<MotionDetector>>,
    /// Segurar a saída até o primeiro keyframe, em vez de mostrar os quadros
    /// incompletos que antecedem ele.
    pub wait_for_keyframe: bool,
}

impl PipelineOptions {
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            latency_ms: config.app.latency_ms,
            protocols: config.app.rtsp_protocols.clone(),
            motion: config
                .motion
                .enabled
                .then(|| Arc::new(MotionDetector::new(&config.motion))),
            wait_for_keyframe: config.app.wait_for_keyframe,
        }
    }
}

/// Pipeline recém-construída.
///
/// Contém o `GdkPaintable`, que **não** é `Send` — por isso o chamador
/// (thread da UI) desmonta o struct e envia apenas o [`CameraPipeline`] para o
/// supervisor assíncrono.
pub struct Built {
    pub paintable: gdk::Paintable,
    pub handle: CameraPipeline,
}

/// Tudo que é `Send`: o que o supervisor precisa para operar a pipeline.
#[derive(Debug, Clone)]
pub struct CameraPipeline {
    pub pipeline: gst::Pipeline,
    pub stats: Arc<StreamStats>,
    /// `rtspsrc` — guardado para trocar a `location` ao alternar de stream.
    src: gst::Element,
    /// Ponto de derivação do stream codificado, usado pela gravação.
    tee_rtp: gst::Element,
}

impl CameraPipeline {
    /// Troca a URL do `rtspsrc`. Só tem efeito com a pipeline em `NULL`/`READY`,
    /// que é justamente o estado entre duas tentativas do supervisor.
    pub fn set_location(&self, url: &str) {
        self.src.set_property("location", url);
    }

    /// Elemento de onde a gravação deriva o stream codificado.
    pub fn recording_tee(&self) -> &gst::Element {
        &self.tee_rtp
    }
}

// ---------------------------------------------------------------------------
// Construção
// ---------------------------------------------------------------------------

pub fn build(camera: &Camera, opts: &PipelineOptions) -> Result<Built> {
    let pipeline = gst::Pipeline::with_name(&format!("pipeline-{}", camera.label()));

    let src = gst::ElementFactory::make("rtspsrc")
        .name("src")
        .property("location", camera.url_for(camera.grid_stream))
        .property("latency", opts.latency_ms)
        .property_from_str("protocols", &opts.protocols)
        // Sem dados por 5 s → o rtspsrc desiste e emite erro no bus, que o
        // supervisor transforma em reconexão com backoff.
        .property("timeout", 5_000_000u64)
        .property("tcp-timeout", 5_000_000u64)
        // Preferimos descartar frames atrasados a acumular latência: é um
        // dashboard ao vivo, não um player.
        .property("drop-on-latency", true)
        .property("do-retransmission", false)
        .property(
            "user-agent",
            concat!("nvr-dashboard/", env!("CARGO_PKG_VERSION")),
        )
        .build()
        .context("elemento `rtspsrc` indisponível (instale gst-plugins-good)")?;

    // `allow-not-linked` mantém o tee vivo enquanto o ramo de gravação não
    // existe, em vez de derrubar a pipeline com "not-linked".
    let tee_rtp = make("tee", "tee-rtp")?;
    tee_rtp.set_property("allow-not-linked", true);

    let decode = make("decodebin", "decode")?;
    let tee_raw = make("tee", "tee-raw")?;
    tee_raw.set_property("allow-not-linked", true);
    let sink = gst::ElementFactory::make("gtk4paintablesink")
        .name("sink")
        .build()
        .context("elemento `gtk4paintablesink` indisponível (instale gst-plugin-gtk4)")?;
    let paintable = sink.property::<gdk::Paintable>("paintable");

    let queue_decode = encoded_queue("q-decode")?;
    let queue_sink = live_queue("q-sink")?;

    pipeline
        .add_many([
            &src,
            &tee_rtp,
            &queue_decode,
            &decode,
            &tee_raw,
            &queue_sink,
            &sink,
        ])
        .context("falha ao montar a pipeline")?;

    gst::Element::link_many([&tee_rtp, &queue_decode, &decode])
        .context("falha ao ligar o tee RTP ao decodebin")?;
    gst::Element::link_many([&tee_raw, &queue_sink, &sink])
        .context("falha ao ligar o tee de vídeo ao sink")?;

    let stats = Arc::new(StreamStats::default());
    attach_sink_probes(&sink, &stats)?;
    attach_bitrate_probe(&tee_rtp, &stats)?;

    if let Some(detector) = &opts.motion {
        attach_motion_branch(&pipeline, &tee_raw, &stats, Arc::clone(detector))
            .context("falha ao montar o ramo de detecção de movimento")?;
    }

    tune_autoplugged_elements(&decode, camera.label(), opts.wait_for_keyframe);
    link_rtspsrc_to_tee(&src, &tee_rtp, camera.label());
    link_decodebin_to_tee(&decode, &tee_raw, camera.label());

    Ok(Built {
        paintable,
        handle: CameraPipeline {
            pipeline,
            stats,
            src,
            tee_rtp,
        },
    })
}

/// Ajusta os elementos que o `decodebin` monta sozinho.
///
/// O NVR de referência usa um GOP longo (mais de 30 s): quem entra no meio do
/// fluxo fica sem o keyframe inicial. Os decodificadores por hardware, nesse
/// caso, entregam a superfície zerada com os blocos que forem chegando — em
/// NV12 isso aparece como uma tela verde que vai "pintando" devagar até o
/// próximo keyframe.
///
/// A correção tem duas frentes, ambas no depayloader:
/// - `wait-for-keyframe`: segura a saída até um quadro completo, então o tile
///   mostra "Aguardando keyframe…" em vez de verde. Basta isso — nada chega ao
///   decodificador antes da hora, e mexer no descarte de quadros do decoder por
///   cima só derrubaria quadros bons depois.
/// - `request-keyframe`: pede um IDR ao NVR por RTCP, para não esperar o GOP
///   inteiro. Nem todo gravador atende, mas não custa nada tentar.
///
/// Cada propriedade é aplicada só onde existe, porque os elementos variam
/// conforme o codec e o decodificador escolhido.
fn tune_autoplugged_elements(decode: &gst::Element, label: String, wait_for_keyframe: bool) {
    let Some(bin) = decode.downcast_ref::<gst::Bin>() else {
        tracing::warn!(camera = %label, "decodebin não é um bin; nada a ajustar");
        return;
    };
    bin.connect_deep_element_added(move |_, _, element| {
        let tuned = [
            ("wait-for-keyframe", wait_for_keyframe),
            // Pedir o IDR não custa nada nem muda o que é exibido.
            ("request-keyframe", true),
        ]
        .into_iter()
        .filter(|(property, value)| set_bool_if_present(element, property, *value))
        .map(|(property, _)| property)
        .collect::<Vec<_>>();

        if !tuned.is_empty() {
            tracing::debug!(
                camera = %label,
                elemento = %element.name(),
                propriedades = ?tuned,
                "elemento autoplugado ajustado"
            );
        }
    });
}

/// Define uma propriedade booleana se o elemento tiver essa propriedade.
fn set_bool_if_present(element: &gst::Element, property: &str, value: bool) -> bool {
    if !element.has_property_with_type(property, bool::static_type()) {
        return false;
    }
    element.set_property(property, value);
    true
}

fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| format!("elemento `{factory}` indisponível"))
}

/// Fila para ramos ao vivo: pequena e com descarte, para o atraso não crescer
/// quando um ramo fica mais lento que o outro.
fn live_queue(name: &str) -> Result<gst::Element> {
    let queue = make("queue", name)?;
    queue.set_property("max-size-buffers", 5u32);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 0u64);
    queue.set_property_from_str("leaky", "downstream");
    Ok(queue)
}

/// Fila para o stream **codificado**, antes do decoder.
///
/// Não pode descartar: perder um único pacote RTP de um keyframe corrompe o
/// quadro e a imagem congela até o próximo IDR — com GOP de 30 s+, é o
/// "travando muito". Quem controla a latência é o jitterbuffer do `rtspsrc`
/// (`drop-on-latency`); aqui só absorvemos rajadas, com teto de 4 MB.
fn encoded_queue(name: &str) -> Result<gst::Element> {
    let queue = make("queue", name)?;
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-bytes", 4 * 1024 * 1024u32);
    queue.set_property("max-size-time", 2 * gst::ClockTime::SECOND.nseconds());
    Ok(queue)
}

/// Liga o pad de vídeo do `rtspsrc` ao tee, ignorando áudio/metadados.
fn link_rtspsrc_to_tee(src: &gst::Element, tee: &gst::Element, label: String) {
    let tee = tee.downgrade();
    src.connect_pad_added(move |_, src_pad| {
        let Some(tee) = tee.upgrade() else { return };
        let Some(sink_pad) = tee.static_pad("sink") else {
            return;
        };

        if !pad_is_video(src_pad) {
            tracing::debug!(camera = %label, pad = %src_pad.name(), "pad não-vídeo ignorado");
            return;
        }
        if sink_pad.is_linked() {
            tracing::debug!(camera = %label, "tee já tem um stream de vídeo");
            return;
        }
        if let Err(err) = src_pad.link(&sink_pad) {
            tracing::error!(camera = %label, %err, "falha ao ligar rtspsrc ao tee");
        }
    });
}

/// Liga a saída decodificada ao tee de vídeo.
///
/// Sem `videoconvert` no caminho de exibição: o `gtk4paintablesink` aceita
/// NV12/DMABuf/GLMemory direto, então o frame decodificado por hardware não é
/// copiado para a CPU. A conversão fica só no ramo de movimento.
fn link_decodebin_to_tee(decode: &gst::Element, tee: &gst::Element, label: String) {
    let convert = tee.downgrade();
    decode.connect_pad_added(move |_, src_pad| {
        let Some(convert) = convert.upgrade() else {
            return;
        };
        let Some(sink_pad) = convert.static_pad("sink") else {
            return;
        };

        let is_video = src_pad
            .current_caps()
            .and_then(|caps| caps.structure(0).map(|s| s.name().starts_with("video/")))
            .unwrap_or(false);
        if !is_video || sink_pad.is_linked() {
            return;
        }
        if let Err(err) = src_pad.link(&sink_pad) {
            tracing::error!(camera = %label, %err, "falha ao ligar decodebin ao tee de vídeo");
        }
    });
}

/// Um pad RTP de vídeo tem caps `application/x-rtp, media=(string)video`.
fn pad_is_video(pad: &gst::Pad) -> bool {
    let Some(caps) = pad.current_caps() else {
        return false;
    };
    let Some(structure) = caps.structure(0) else {
        return false;
    };
    match structure.get::<String>("media") {
        Ok(media) => media == "video",
        // Sem campo `media` (ex.: caps já elementares) caímos no nome.
        Err(_) => structure.name().starts_with("video/"),
    }
}

// ---------------------------------------------------------------------------
// Probes
// ---------------------------------------------------------------------------

/// Contadores de frames e resolução, no pad de entrada do sink.
fn attach_sink_probes(sink: &gst::Element, stats: &Arc<StreamStats>) -> Result<()> {
    let pad = sink
        .static_pad("sink")
        .context("gtk4paintablesink sem pad `sink`")?;

    let frame_stats = Arc::clone(stats);
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        frame_stats.note_frame();
        gst::PadProbeReturn::Ok
    });

    let caps_stats = Arc::clone(stats);
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(event)) = &info.data
            && let gst::EventView::Caps(caps_event) = event.view()
            && let Some(structure) = caps_event.caps().structure(0)
            && let (Ok(width), Ok(height)) = (
                structure.get::<i32>("width"),
                structure.get::<i32>("height"),
            )
        {
            // As caps completas ajudam a diagnosticar problemas de formato /
            // memória (VA-API, DMABuf) que aparecem como imagem corrompida.
            tracing::debug!(caps = %caps_event.caps(), "caps negociadas no sink");
            caps_stats.set_resolution(width, height);
        }
        gst::PadProbeReturn::Ok
    });

    Ok(())
}

/// Conta bytes de RTP na entrada do tee — é o tráfego real vindo do NVR,
/// antes de decodificar.
fn attach_bitrate_probe(tee: &gst::Element, stats: &Arc<StreamStats>) -> Result<()> {
    let pad = tee.static_pad("sink").context("tee sem pad `sink`")?;
    let stats = Arc::clone(stats);
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
            stats.note_bytes(buffer.size() as u64);
        }
        gst::PadProbeReturn::Ok
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Ramo de detecção de movimento
// ---------------------------------------------------------------------------

/// Deriva uma cópia minúscula em tons de cinza e roda o diff de quadros nela.
fn attach_motion_branch(
    pipeline: &gst::Pipeline,
    tee_raw: &gst::Element,
    stats: &Arc<StreamStats>,
    detector: Arc<MotionDetector>,
) -> Result<()> {
    let queue = live_queue("q-motion")?;
    let convert = make("videoconvert", "motion-convert")?;
    let scale = make("videoscale", "motion-scale")?;
    let rate = make("videorate", "motion-rate")?;
    let filter = make("capsfilter", "motion-caps")?;
    filter.set_property(
        "caps",
        gst::Caps::builder("video/x-raw")
            .field("format", "GRAY8")
            .field("width", MOTION_WIDTH)
            .field("height", MOTION_HEIGHT)
            .field("framerate", gst::Fraction::new(MOTION_FPS, 1))
            .build(),
    );
    let sink = make("fakesink", "motion-sink")?;
    sink.set_property("sync", false);
    sink.set_property("async", false);

    let elements = [&queue, &convert, &scale, &rate, &filter, &sink];
    pipeline.add_many(elements)?;
    gst::Element::link_many(elements)?;
    tee_raw.link(&queue)?;

    let pad = sink.static_pad("sink").context("fakesink sem pad `sink`")?;
    let stats = Arc::clone(stats);
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data
            && let Ok(map) = buffer.map_readable()
            && detector.feed(map.as_slice())
        {
            stats.note_motion();
        }
        gst::PadProbeReturn::Ok
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// Seleção de decodificador
// ---------------------------------------------------------------------------

/// Plugins cujos elementos `*dec` fazem decodificação acelerada por hardware.
const HARDWARE_DECODER_PLUGINS: &[&str] = &["va", "nvcodec", "vaapi", "msdk"];

/// Ajusta o registro do GStreamer conforme a preferência do usuário.
///
/// Em distros recentes os decoders VA-API/NVDEC já registram rank `primary+1`,
/// então o caminho interessante é o inverso: rebaixá-los quando o usuário quer
/// forçar software (útil para isolar problemas de driver em GPU híbrida).
pub fn configure_decoders(hardware: bool) {
    let registry = gst::Registry::get();
    let mut touched = Vec::new();

    for plugin in HARDWARE_DECODER_PLUGINS {
        for feature in registry.features_by_plugin(plugin) {
            if !feature.name().ends_with("dec") {
                continue;
            }
            if hardware {
                touched.push(feature.name().to_string());
            } else if feature.rank() != gst::Rank::NONE {
                feature.set_rank(gst::Rank::NONE);
                touched.push(feature.name().to_string());
            }
        }
    }

    if hardware {
        tracing::info!(
            disponiveis = touched.len(),
            "decodificação por hardware habilitada (ranks do registro preservados)"
        );
    } else {
        tracing::info!(
            rebaixados = touched.len(),
            "decodificação por hardware desabilitada; usando decoders de software"
        );
    }
    tracing::debug!(elementos = ?touched, "decoders de hardware detectados");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_contam_frames_bytes_e_atividade() {
        let stats = StreamStats::default();
        assert_eq!(stats.frames(), 0);
        assert_eq!(stats.bytes(), 0);
        assert_eq!(stats.resolution(), None);

        stats.note_frame();
        stats.note_frame();
        stats.note_bytes(1500);
        assert_eq!(stats.frames(), 2);
        assert_eq!(stats.session_frames(), 2);
        assert_eq!(stats.bytes(), 1500);
        assert!(stats.idle_for() < Duration::from_secs(1));
        assert!(stats.data_idle_for() < Duration::from_secs(1));

        stats.set_resolution(1920, 1080);
        assert_eq!(stats.resolution(), Some((1920, 1080)));
    }

    #[test]
    fn note_attempt_reinicia_o_relogio_do_watchdog() {
        let stats = StreamStats::default();
        // Sem nenhuma atividade o relógio conta desde o início do processo.
        let before = stats.idle_for();
        stats.note_frame();
        stats.set_resolution(640, 360);

        stats.note_attempt();
        assert!(stats.idle_for() <= before);
        assert_eq!(stats.session_frames(), 0, "a nova tentativa começa do zero");
        assert_eq!(
            stats.resolution(),
            None,
            "resolução antiga não vaza para a nova sessão"
        );
        assert_eq!(stats.frames(), 1, "o total acumulado é preservado");
    }

    #[test]
    fn relogio_de_dados_e_de_quadros_sao_independentes() {
        // É o que impede o watchdog de reiniciar a pipeline em loop enquanto
        // ela espera o keyframe: chegam bytes, mas nenhum quadro.
        let stats = StreamStats::default();
        stats.note_attempt();
        std::thread::sleep(Duration::from_millis(30));

        stats.note_bytes(1200);
        assert!(
            stats.data_idle_for() < stats.idle_for(),
            "receber bytes renova só o relógio de dados"
        );
        assert_eq!(stats.session_frames(), 0);

        stats.note_frame();
        assert!(
            stats.idle_for() <= Duration::from_millis(5),
            "o quadro renova o outro relógio"
        );
    }

    #[test]
    fn movimento_expira_com_o_tempo() {
        let stats = StreamStats::default();
        assert!(
            !stats.motion_recent(Duration::from_secs(60)),
            "nada aconteceu ainda"
        );

        stats.note_motion();
        assert_eq!(stats.motion_events(), 1);
        assert!(stats.motion_recent(Duration::from_secs(60)));
        assert!(
            !stats.motion_recent(Duration::ZERO),
            "janela vazia nunca casa"
        );
    }
}

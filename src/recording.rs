//! Gravação local sob demanda, derivada do stream **codificado**.
//!
//! O ramo é anexado ao `tee` que fica logo depois do `rtspsrc`, antes de
//! decodificar: gravamos exatamente os bytes que o NVR mandou, sem recodificar
//! e sem abrir uma segunda conexão RTSP.
//!
//! ```text
//!  tee_rtp ─▶ queue ─▶ parsebin ─▶ splitmuxsink   (bin adicionado em runtime)
//! ```
//!
//! `splitmuxsink` fatia a gravação em arquivos de duração fixa e, com
//! `max-files`, apaga os mais antigos — o buffer circular pedido no plano.
//!
//! Parar é o passo delicado: soltar o ramo sem fechar o arquivo deixaria um
//! vídeo truncado. A sequência é a canônica do GStreamer para remoção dinâmica:
//! bloquear o pad do tee com um probe `IDLE`, desconectar, injetar `EOS` no
//! ramo e só remover o bin quando o `EOS` voltar pelo bus — daí o
//! `message-forward` no bin e o par [`Recording::begin_stop`] /
//! [`Recording::finish`].

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gst::prelude::*;
use gtk::glib;

use crate::camera::Camera;
use crate::config::Config;

/// Se o `EOS` não voltar nesse prazo, removemos o ramo assim mesmo — melhor um
/// último arquivo possivelmente truncado do que um bin preso na pipeline.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, Clone)]
pub struct RecordingOptions {
    pub directory: PathBuf,
    pub segment: Duration,
    /// `0` = sem limite (não vira buffer circular).
    pub max_files: u32,
    pub muxer: &'static str,
    pub extension: &'static str,
}

impl RecordingOptions {
    pub fn from_config(config: &Config) -> Self {
        // Matroska tolera melhor uma interrupção abrupta; MP4 é mais portátil.
        let (muxer, extension) = match config.recording.container.as_str() {
            "mp4" => ("mp4mux", "mp4"),
            _ => ("matroskamux", "mkv"),
        };
        Self {
            directory: config.recording_dir(),
            segment: Duration::from_secs(config.recording.segment_seconds),
            max_files: config.recording.max_files,
            muxer,
            extension,
        }
    }
}

/// Ramo de gravação vivo, anexado a uma pipeline em execução.
#[derive(Debug)]
pub struct Recording {
    bin: gst::Bin,
    tee_pad: gst::Pad,
    ghost: gst::Pad,
    /// Padrão de arquivos (contém `%05d`), para exibir na UI e no log.
    pub pattern: String,
    pub started_at: Instant,
    /// Quando `begin_stop` foi chamado, para o timeout de segurança.
    stopping_since: Option<Instant>,
}

impl Recording {
    /// Há quanto tempo esta gravação está rodando.
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// O `EOS` demorou demais para voltar?
    pub fn stop_timed_out(&self) -> bool {
        self.stopping_since
            .is_some_and(|since| since.elapsed() > STOP_TIMEOUT)
    }

    /// Solta o ramo do tee e injeta `EOS` para fechar o arquivo.
    ///
    /// Não remove o bin: isso é feito em [`Recording::finish`], depois que o
    /// `EOS` percorrer o ramo inteiro.
    pub fn begin_stop(&mut self, tee: &gst::Element) {
        if self.stopping_since.is_some() {
            return;
        }
        self.stopping_since = Some(Instant::now());

        let tee = tee.clone();
        let ghost = self.ghost.clone();
        // Um probe IDLE só roda quando não há buffer atravessando o pad, então
        // desconectar aqui não corta um frame pela metade.
        self.tee_pad
            .add_probe(gst::PadProbeType::IDLE, move |pad, _| {
                let _ = pad.unlink(&ghost);
                tee.release_request_pad(pad);
                ghost.send_event(gst::event::Eos::new());
                gst::PadProbeReturn::Remove
            });
    }

    /// Esta mensagem do bus é o `EOS` deste ramo?
    ///
    /// Com `message-forward`, o bin reempacota o `EOS` dos filhos numa mensagem
    /// `GstBinForwarded` postada no bus da pipeline.
    pub fn matches_eos(&self, message: &gst::Message) -> bool {
        let gst::MessageView::Element(element) = message.view() else {
            return false;
        };
        if message.src().map(|src| src.name()) != Some(self.bin.name()) {
            return false;
        }
        let Some(structure) = element.structure() else {
            return false;
        };
        if structure.name() != "GstBinForwarded" {
            return false;
        }
        structure
            .get::<gst::Message>("message")
            .is_ok_and(|inner| matches!(inner.view(), gst::MessageView::Eos(_)))
    }

    /// Este objeto do GStreamer faz parte do ramo de gravação?
    ///
    /// Usado para separar "disco cheio" (erro só da gravação) de "o stream
    /// caiu" (erro que precisa de reconexão).
    pub fn owns(&self, object: &gst::Object) -> bool {
        object.has_as_ancestor(&self.bin)
    }

    /// Remove o bin da pipeline. Chamar depois do `EOS` (ou do timeout).
    pub fn finish(self, pipeline: &gst::Pipeline) -> Result<()> {
        self.bin
            .set_state(gst::State::Null)
            .context("falha ao parar o ramo de gravação")?;
        pipeline
            .remove(&self.bin)
            .context("falha ao remover o ramo de gravação")?;
        Ok(())
    }
}

/// Anexa um ramo de gravação a uma pipeline que já está rodando.
pub fn start(
    pipeline: &gst::Pipeline,
    tee: &gst::Element,
    camera: &Camera,
    options: &RecordingOptions,
) -> Result<Recording> {
    std::fs::create_dir_all(&options.directory)
        .with_context(|| format!("não consegui criar {}", options.directory.display()))?;

    let stamp = glib::DateTime::now_local()
        .and_then(|now| now.format("%Y%m%d-%H%M%S"))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| "sem-data".to_string());
    let pattern = options
        .directory
        .join(format!(
            "{}_{stamp}_%05d.{}",
            camera.slug(),
            options.extension
        ))
        .to_string_lossy()
        .into_owned();

    let bin = gst::Bin::with_name(&format!("rec-{}", camera.label()));
    // Sem isso o EOS do splitmuxsink morreria dentro do bin e nunca saberíamos
    // que o arquivo terminou de ser finalizado.
    bin.set_property("message-forward", true);

    let queue = gst::ElementFactory::make("queue")
        .name("rec-queue")
        // Ao contrário dos ramos ao vivo, aqui não descartamos nada: um frame
        // perdido é um buraco no arquivo gravado.
        .property("max-size-buffers", 0u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 3_000_000_000u64)
        .build()
        .context("elemento `queue` indisponível")?;

    // `parsebin` faz depayload + parse sem decodificar, então o vídeo vai para
    // o disco exatamente como veio do NVR.
    let parse = gst::ElementFactory::make("parsebin")
        .name("rec-parse")
        .build()
        .context("elemento `parsebin` indisponível (instale gst-plugins-base)")?;

    let splitmux = gst::ElementFactory::make("splitmuxsink")
        .name("rec-sink")
        .property("location", &pattern)
        .property("max-size-time", options.segment.as_nanos() as u64)
        .property("max-files", options.max_files)
        // `muxer-factory` só é considerado com finalização assíncrona.
        .property("async-finalize", true)
        .property("muxer-factory", options.muxer)
        .build()
        .context("elemento `splitmuxsink` indisponível (instale gst-plugins-good)")?;

    bin.add_many([&queue, &parse, &splitmux])?;
    queue
        .link(&parse)
        .context("falha ao ligar queue ao parsebin")?;
    link_parsebin_to_splitmux(&parse, &splitmux, camera.label());

    let queue_sink = queue
        .static_pad("sink")
        .context("queue de gravação sem pad `sink`")?;
    let ghost = gst::GhostPad::with_target(&queue_sink)
        .context("falha ao criar o ghost pad do ramo de gravação")?;
    ghost.set_active(true)?;
    bin.add_pad(&ghost)?;

    pipeline
        .add(&bin)
        .context("falha ao adicionar o ramo de gravação à pipeline")?;

    let tee_pad = tee
        .request_pad_simple("src_%u")
        .context("o tee não cedeu um pad para a gravação")?;
    let ghost_pad: gst::Pad = ghost.upcast();
    tee_pad
        .link(&ghost_pad)
        .context("falha ao ligar o tee ao ramo de gravação")?;

    // Só agora o ramo entra em PLAYING, já ligado e pronto para receber dados.
    bin.sync_state_with_parent()
        .context("o ramo de gravação não acompanhou o estado da pipeline")?;

    tracing::info!(camera = %camera.label(), arquivos = %pattern, "gravação iniciada");

    Ok(Recording {
        bin,
        tee_pad,
        ghost: ghost_pad,
        pattern,
        started_at: Instant::now(),
        stopping_since: None,
    })
}

/// Liga a saída do `parsebin` ao `splitmuxsink`.
///
/// Diferente do `decodebin`, o `parsebin` pode expor o pad **antes** de
/// negociar as caps — decidir "é vídeo?" só com `current_caps()` faz o link
/// nunca acontecer e a gravação sair vazia. Quando as caps ainda não existem,
/// esperamos o evento `CAPS` no próprio pad.
fn link_parsebin_to_splitmux(parse: &gst::Element, splitmux: &gst::Element, label: String) {
    let splitmux_weak = splitmux.downgrade();
    parse.connect_pad_added(move |_, src_pad| {
        let Some(splitmux) = splitmux_weak.upgrade() else {
            return;
        };
        match src_pad.current_caps().map(|caps| caps_is_video(&caps)) {
            Some(true) => link_video_pad(&splitmux, src_pad, &label),
            Some(false) => {
                tracing::debug!(camera = %label, "pad não-vídeo ignorado na gravação");
            }
            None => {
                tracing::debug!(camera = %label, pad = %src_pad.name(), "aguardando caps do parsebin");
                await_caps_then_link(src_pad, &splitmux, label.clone());
            }
        }
    });
}

/// Instala um probe de uso único que liga o pad assim que as caps chegarem.
fn await_caps_then_link(src_pad: &gst::Pad, splitmux: &gst::Element, label: String) {
    let splitmux_weak = splitmux.downgrade();
    src_pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
        let Some(gst::PadProbeData::Event(event)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(caps_event) = event.view() else {
            return gst::PadProbeReturn::Ok;
        };
        // As caps vêm do evento: o pad ainda não guardou o sticky event.
        if caps_is_video(caps_event.caps())
            && let Some(splitmux) = splitmux_weak.upgrade()
        {
            link_video_pad(&splitmux, pad, &label);
        }
        // `Remove` deixa o evento seguir para o peer recém-ligado.
        gst::PadProbeReturn::Remove
    });
}

fn caps_is_video(caps: &gst::CapsRef) -> bool {
    caps.structure(0)
        .is_some_and(|structure| structure.name().starts_with("video/"))
}

/// Pede o pad `video` do `splitmuxsink` e liga. Idempotente.
fn link_video_pad(splitmux: &gst::Element, src_pad: &gst::Pad, label: &str) {
    if splitmux.static_pad("video").is_some() {
        tracing::debug!(camera = %label, "ramo de gravação já tem vídeo");
        return;
    }
    let Some(sink_pad) = splitmux.request_pad_simple("video") else {
        tracing::error!(camera = %label, "splitmuxsink não cedeu o pad `video`");
        return;
    };
    match src_pad.link(&sink_pad) {
        Ok(_) => {
            tracing::debug!(camera = %label, caps = ?src_pad.current_caps(), "gravação ligada ao muxer")
        }
        Err(err) => {
            tracing::error!(camera = %label, %err, "falha ao ligar parsebin ao splitmuxsink")
        }
    }
}

//! Áudio ao vivo de uma câmera: escutar o som que o NVR já manda no RTSP.
//!
//! O `rtspsrc` expõe um segundo pad (áudio, G.711 nos NVRs iCSee). Ele fica
//! **desligado** até o usuário pedir para ouvir; só então montamos
//! `queue → decodebin → audioconvert → audioresample → autoaudiosink` e ligamos
//! ao pad. Desligar desmonta tudo, então nada abre o dispositivo de som (nem
//! consome CPU) enquanto ninguém escuta.
//!
//! O ramo vive num `gst::Bin` próprio (`audio-bin`) para que o supervisor
//! consiga reconhecer erros dele e não reiniciar o vídeo por causa do som.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use gst::prelude::*;

const BIN_NAME: &str = "audio-bin";

#[derive(Debug, Default)]
struct Inner {
    /// Pad de áudio do `rtspsrc` da conexão atual.
    pad: Option<gst::Pad>,
    /// Ramo montado, se estiver escutando.
    bin: Option<gst::Bin>,
}

/// Estado do áudio de uma câmera. Compartilhado entre a UI (liga/desliga) e o
/// callback de `pad-added` do `rtspsrc` (religa após reconexões).
#[derive(Debug, Default)]
pub struct AudioState {
    wanted: AtomicBool,
    inner: Mutex<Inner>,
}

impl AudioState {
    /// O usuário quer ouvir esta câmera?
    pub fn is_listening(&self) -> bool {
        self.wanted.load(Ordering::Relaxed)
    }

    /// A conexão atual tem faixa de áudio?
    pub fn has_audio(&self) -> bool {
        self.inner
            .lock()
            .unwrap()
            .pad
            .as_ref()
            .is_some_and(|pad| pad.parent().is_some())
    }

    /// Liga ou desliga a escuta. Sem faixa de áudio ainda, o pedido fica
    /// guardado e vale assim que ela aparecer (ex.: durante a reconexão).
    pub fn set_listening(&self, pipeline: &gst::Pipeline, on: bool, label: &str) {
        self.wanted.store(on, Ordering::Relaxed);
        let mut inner = self.inner.lock().unwrap();
        if on {
            self.attach(pipeline, &mut inner, label);
        } else {
            detach(pipeline, &mut inner);
        }
    }

    /// Chamado quando o `rtspsrc` (re)expõe o pad de áudio.
    pub fn on_pad_added(&self, pipeline: &gst::Pipeline, pad: &gst::Pad, label: &str) {
        let mut inner = self.inner.lock().unwrap();
        // Restos da conexão anterior.
        detach(pipeline, &mut inner);
        inner.pad = Some(pad.clone());
        if self.is_listening() {
            self.attach(pipeline, &mut inner, label);
        }
    }

    /// O erro veio do ramo de áudio? Então não é motivo para reiniciar o vídeo.
    pub fn owns(&self, source: &gst::Object) -> bool {
        self.inner.lock().unwrap().bin.as_ref().is_some_and(|bin| {
            source == bin.upcast_ref::<gst::Object>() || source.has_as_ancestor(bin)
        })
    }

    /// O ramo de áudio falhou: desmonta e deixa de tentar.
    pub fn fail(&self, pipeline: &gst::Pipeline) {
        self.wanted.store(false, Ordering::Relaxed);
        detach(pipeline, &mut self.inner.lock().unwrap());
    }

    fn attach(&self, pipeline: &gst::Pipeline, inner: &mut Inner, label: &str) {
        if inner.bin.is_some() {
            return;
        }
        let Some(pad) = inner.pad.clone().filter(|pad| pad.parent().is_some()) else {
            return;
        };
        match build_and_link(pipeline, &pad) {
            Ok(bin) => inner.bin = Some(bin),
            Err(err) => {
                tracing::warn!(camera = %label, erro = %format!("{err:#}"), "não consegui ligar o áudio");
                self.wanted.store(false, Ordering::Relaxed);
            }
        }
    }
}

fn build_and_link(pipeline: &gst::Pipeline, pad: &gst::Pad) -> Result<gst::Bin> {
    let make = |factory: &str| {
        gst::ElementFactory::make(factory)
            .build()
            .with_context(|| format!("elemento `{factory}` indisponível"))
    };
    let queue = make("queue")?;
    // Descarta o excesso em vez de acumular atraso: é som ao vivo.
    queue.set_property("max-size-buffers", 0u32);
    queue.set_property("max-size-bytes", 0u32);
    queue.set_property("max-size-time", 1_000_000_000u64);
    queue.set_property_from_str("leaky", "downstream");
    let decode = make("decodebin")?;
    let convert = make("audioconvert")?;
    let resample = make("audioresample")?;
    let sink = make("autoaudiosink")?;

    // Abre o dispositivo de som antes de entrar na pipeline: se falhar, o erro
    // não chega ao bus do vídeo.
    sink.set_state(gst::State::Ready)
        .context("sem saída de áudio disponível")?;

    let bin = gst::Bin::with_name(BIN_NAME);
    bin.add_many([&queue, &decode, &convert, &resample, &sink])?;
    queue.link(&decode)?;
    gst::Element::link_many([&convert, &resample, &sink])?;

    let convert_weak = convert.downgrade();
    decode.connect_pad_added(move |_, src_pad| {
        let Some(convert) = convert_weak.upgrade() else {
            return;
        };
        let Some(sink_pad) = convert.static_pad("sink") else {
            return;
        };
        let is_audio = src_pad
            .current_caps()
            .and_then(|caps| caps.structure(0).map(|s| s.name().starts_with("audio/")))
            .unwrap_or(false);
        if is_audio && !sink_pad.is_linked() {
            let _ = src_pad.link(&sink_pad);
        }
    });

    let input = queue.static_pad("sink").context("queue sem pad `sink`")?;
    let ghost = gst::GhostPad::with_target(&input)?;
    ghost.set_active(true)?;
    bin.add_pad(&ghost)?;

    pipeline.add(&bin)?;
    if bin.sync_state_with_parent().is_err() {
        let _ = bin.set_state(gst::State::Null);
        let _ = pipeline.remove(&bin);
        anyhow::bail!("falha ao iniciar o ramo de áudio");
    }
    // Diagnóstico: registra quando o primeiro pacote de som chega.
    let seen = AtomicBool::new(false);
    ghost.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        if !seen.swap(true, Ordering::Relaxed) {
            tracing::info!("áudio recebendo dados");
        }
        gst::PadProbeReturn::Ok
    });
    pad.link(&ghost)
        .map_err(|err| anyhow::anyhow!("falha ao ligar o áudio: {err:?}"))?;
    Ok(bin)
}

fn detach(pipeline: &gst::Pipeline, inner: &mut Inner) {
    let Some(bin) = inner.bin.take() else {
        return;
    };
    if let (Some(pad), Some(sink)) = (inner.pad.as_ref(), bin.static_pad("sink")) {
        let _ = pad.unlink(&sink);
    }
    let _ = bin.set_state(gst::State::Null);
    let _ = pipeline.remove(&bin);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estado_inicial_e_pedido_sem_faixa() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let audio = AudioState::default();
        assert!(!audio.is_listening());
        assert!(!audio.has_audio());

        // Sem faixa de áudio o pedido fica guardado, sem erro.
        audio.set_listening(&pipeline, true, "teste");
        assert!(audio.is_listening());
        audio.set_listening(&pipeline, false, "teste");
        assert!(!audio.is_listening());
    }

    #[test]
    fn erro_do_ramo_desliga_o_pedido() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let audio = AudioState::default();
        audio.set_listening(&pipeline, true, "teste");
        audio.fail(&pipeline);
        assert!(!audio.is_listening());
    }

    #[test]
    fn ramo_de_audio_monta_e_desmonta_com_fonte_sintetica() {
        gst::init().unwrap();
        // Uma fonte de áudio de mentira faz o papel do pad do `rtspsrc`.
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("audiotestsrc")
            .property("is-live", true)
            .build()
            .unwrap();
        pipeline.add(&src).unwrap();
        let pad = src.static_pad("src").unwrap();

        let audio = AudioState::default();
        audio.inner.lock().unwrap().pad = Some(pad);
        // Sem dispositivo de som no ambiente de teste, montar pode falhar de
        // forma limpa (`wanted` volta a falso); o que não pode é travar.
        audio.set_listening(&pipeline, true, "teste");
        audio.set_listening(&pipeline, false, "teste");
        assert!(!audio.is_listening());
        assert!(pipeline.by_name(BIN_NAME).is_none(), "ramo removido");
    }
}

//! Supervisão assíncrona de cada pipeline: detecção de queda, health-check,
//! reconexão com backoff exponencial, troca de stream e ciclo de gravação.
//!
//! Um [`Supervisor`] por câmera roda como task do tokio, fora da thread da UI.
//! Ele consome o bus do GStreamer como stream assíncrono e mantém um watchdog
//! de 1 s baseado em `StreamStats` — assim uma câmera que "congela" sem emitir
//! erro no bus também é detectada. Mudanças de estado vão para a UI por um
//! `async_channel`, e comandos da UI voltam por outro.

use std::convert::Infallible;
use std::time::Duration;

use futures_util::StreamExt;
use gst::MessageView;
use gst::prelude::*;

use crate::camera::{Camera, Redactor};
use crate::pipeline::CameraPipeline;
use crate::recording::{self, Recording, RecordingOptions};

/// Fecha-se quando o app está saindo; todos os supervisores acordam juntos.
pub type ShutdownSignal = async_channel::Receiver<Infallible>;

/// Mensagens do bus que interessam ao supervisor.
///
/// `Element` entra por causa do `GstBinForwarded` que sinaliza o fim da
/// finalização de um arquivo de gravação.
const WATCHED_MESSAGES: &[gst::MessageType] = &[
    gst::MessageType::Error,
    gst::MessageType::Eos,
    gst::MessageType::Warning,
    gst::MessageType::Element,
];

/// Quanto esperamos por um TCP connect ao NVR no health-check.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Período do watchdog e do serviço da máquina de estados da gravação.
const TICK: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Estado observável pela UI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraState {
    /// Tentando estabelecer o stream (handshake RTSP, preroll).
    Connecting,
    /// Conectado e recebendo bytes, mas ainda sem um quadro completo: o
    /// decodificador espera o próximo keyframe. Em NVR com GOP longo isso
    /// pode levar dezenas de segundos.
    WaitingKeyframe,
    /// Recebendo frames.
    Live,
    /// Caiu; aguardando o próximo retry.
    Reconnecting {
        attempt: u32,
        retry_in: Duration,
        reason: String,
        /// `false` quando nem o TCP do NVR responde (rede/NVR fora do ar).
        nvr_reachable: bool,
    },
    /// Erro de construção da pipeline — não há retry automático.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingStatus {
    Started { pattern: String },
    Stopped,
    Failed(String),
}

/// Algo mudou numa câmera. Vai do supervisor para a thread da UI.
#[derive(Debug, Clone)]
pub struct CameraEvent {
    pub camera_id: usize,
    pub kind: EventKind,
}

#[derive(Debug, Clone)]
pub enum EventKind {
    State(CameraState),
    Recording(RecordingStatus),
    /// O detector de movimento disparou.
    Motion,
}

/// Ordens da UI para o supervisor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Passa a usar este índice de stream (grid ↔ fullscreen).
    UseStream(u8),
    StartRecording,
    StopRecording,
}

// ---------------------------------------------------------------------------
// Backoff
// ---------------------------------------------------------------------------

/// Backoff exponencial simples: `initial * 2^tentativa`, limitado a `max`.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
}

impl Backoff {
    pub fn new(initial_secs: u64, max_secs: u64) -> Self {
        let initial = Duration::from_secs(initial_secs.max(1));
        Self {
            initial,
            max: Duration::from_secs(max_secs).max(initial),
        }
    }

    /// Espera antes da tentativa `attempt` (base 0).
    pub fn delay(&self, attempt: u32) -> Duration {
        // 2^32 s já estoura qualquer teto razoável; o clamp evita overflow.
        let factor = 1u64 << attempt.min(32);
        self.initial
            .saturating_mul(factor.min(u32::MAX as u64) as u32)
            .min(self.max)
    }
}

// ---------------------------------------------------------------------------
// Gravação
// ---------------------------------------------------------------------------

/// Máquina de estados da gravação de uma câmera.
///
/// `wanted` sobrevive a reconexões de propósito: se o stream cair no meio de
/// uma gravação, ela recomeça sozinha (num arquivo novo) assim que o vídeo
/// voltar, em vez de morrer silenciosamente.
#[derive(Debug, Default)]
struct RecordingState {
    wanted: bool,
    active: Option<Recording>,
    /// Ramo já desconectado, esperando o `EOS` fechar o arquivo.
    stopping: Option<Recording>,
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// Por que a pipeline parou.
enum Outcome {
    /// App encerrando.
    Shutdown,
    /// Stream perdido; `was_live` indica se chegou a entregar frames.
    Lost { reason: String, was_live: bool },
    /// Troca de stream pedida pela UI — reinício imediato, sem backoff.
    Restart,
}

pub struct Supervisor {
    pub camera: Camera,
    pub handle: CameraPipeline,
    pub backoff: Backoff,
    /// Sem dados (ou, já ao vivo, sem quadros) por este tempo, a pipeline é
    /// considerada travada.
    pub stall_timeout: Duration,
    /// Teto para o intervalo "recebendo dados, nenhum quadro decodificável".
    pub keyframe_timeout: Duration,
    pub redactor: Redactor,
    pub recording_options: RecordingOptions,
    pub events: async_channel::Sender<CameraEvent>,
    pub commands: async_channel::Receiver<Command>,
    pub shutdown: ShutdownSignal,
    /// Guarda de vida, nunca lida: existe só para ser dropada no fim de
    /// [`Supervisor::run`]. Quando o último supervisor sai, o canal fecha e o
    /// `main` sabe que pode encerrar o processo sem cortar a finalização de um
    /// arquivo de gravação.
    pub _done_guard: async_channel::Sender<Infallible>,
}

impl Supervisor {
    /// Mantém a câmera no ar até o app encerrar.
    pub async fn run(self) {
        let label = self.camera.label();
        tracing::info!(
            camera = %label,
            nome = %self.camera.name,
            nvr = %self.camera.nvr_id,
            url = %self.camera.masked_url_for(self.camera.grid_stream),
            "supervisor iniciado"
        );

        // O bus stream é criado uma única vez: `BusStream` instala um sync
        // handler no bus e o remove ao ser dropado, então recriá-lo a cada
        // tentativa seria tanto custoso quanto sujeito a corrida.
        let Some(bus) = self.handle.pipeline.bus() else {
            tracing::error!(camera = %label, "pipeline sem bus");
            return;
        };
        let mut messages = bus.stream_filtered(WATCHED_MESSAGES);
        let mut recording = RecordingState::default();
        let mut stream = self.camera.grid_stream;

        let mut attempt: u32 = 0;
        loop {
            if self.emit_state(CameraState::Connecting).await.is_err() {
                break;
            }
            // A pipeline está em NULL neste ponto — o único estado em que o
            // `rtspsrc` aceita uma `location` nova. Aplicar a troca de stream
            // com a pipeline rodando é silenciosamente ignorado.
            self.handle.set_location(&self.camera.url_for(stream));
            self.handle.stats.note_attempt();

            if let Err(err) = self.handle.pipeline.set_state(gst::State::Playing) {
                tracing::error!(camera = %label, %err, "não consegui iniciar a pipeline");
            }

            let outcome = self
                .watch(&mut messages, &mut recording, &mut stream, &label)
                .await;
            self.teardown_recording(&mut messages, &mut recording, &label)
                .await;
            let _ = self.handle.pipeline.set_state(gst::State::Null);

            match outcome {
                Outcome::Shutdown => break,
                // Troca de stream não é falha: reconecta na hora, sem backoff.
                Outcome::Restart => {
                    attempt = 0;
                    continue;
                }
                Outcome::Lost { reason, was_live } => {
                    // Uma conexão que chegou a funcionar zera o backoff: quedas
                    // esporádicas não devem empurrar o retry para o teto.
                    if was_live {
                        attempt = 0;
                    }
                    let retry_in = self.backoff.delay(attempt);
                    attempt = attempt.saturating_add(1);

                    let nvr_reachable =
                        probe_tcp(&self.camera.host, self.camera.port, PROBE_TIMEOUT).await;
                    tracing::warn!(
                        camera = %label,
                        motivo = %reason,
                        tentativa = attempt,
                        retry_em_s = retry_in.as_secs(),
                        nvr_acessivel = nvr_reachable,
                        "stream caiu; reconexão agendada"
                    );

                    let state = CameraState::Reconnecting {
                        attempt,
                        retry_in,
                        reason,
                        nvr_reachable,
                    };
                    if self.emit_state(state).await.is_err()
                        || !self.sleep_or_shutdown(retry_in).await
                    {
                        break;
                    }
                }
            }
        }
        tracing::info!(camera = %label, "supervisor encerrado");
    }

    /// Roda até o stream cair, a UI pedir outro stream, ou o app encerrar.
    async fn watch(
        &self,
        messages: &mut (impl StreamExt<Item = gst::Message> + Unpin),
        recording: &mut RecordingState,
        stream: &mut u8,
        label: &str,
    ) -> Outcome {
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut live = false;
        let mut waiting_keyframe = false;
        let mut seen_motion = self.handle.stats.motion_events();
        // Base para saber se já está chegando dado do NVR nesta tentativa.
        let bytes_at_start = self.handle.stats.bytes();

        loop {
            tokio::select! {
                _ = self.shutdown.recv() => return Outcome::Shutdown,

                command = self.commands.recv() => {
                    match command {
                        // Sem mudança real não reiniciamos: abrir e fechar o
                        // fullscreen sem `adaptive_stream` não deve piscar o vídeo.
                        Ok(Command::UseStream(wanted)) if wanted != *stream => {
                            tracing::info!(
                                camera = %label,
                                de = *stream,
                                para = wanted,
                                url = %self.camera.masked_url_for(wanted),
                                "trocando de stream"
                            );
                            *stream = wanted;
                            return Outcome::Restart;
                        }
                        Ok(Command::UseStream(_)) => {}
                        Ok(Command::StartRecording) => recording.wanted = true,
                        Ok(Command::StopRecording) => recording.wanted = false,
                        // A UI sumiu: o app está encerrando.
                        Err(_) => return Outcome::Shutdown,
                    }
                }

                message = messages.next() => {
                    let Some(message) = message else {
                        return Outcome::Lost { reason: "bus fechado".into(), was_live: live };
                    };
                    if let Some(outcome) = self
                        .handle_message(&message, recording, label, live)
                        .await
                    {
                        return outcome;
                    }
                }

                _ = ticker.tick() => {
                    if let Some(outcome) = self.check_liveness(live) {
                        return outcome;
                    }
                    // Bytes chegando e nenhum quadro ainda = esperando keyframe.
                    // Dizer isso é bem mais útil que um "Conectando…" eterno.
                    if !live
                        && !waiting_keyframe
                        && self.handle.stats.session_frames() == 0
                        && self.handle.stats.bytes() > bytes_at_start
                    {
                        waiting_keyframe = true;
                        tracing::info!(camera = %label, "recebendo dados; aguardando keyframe");
                        if self.emit_state(CameraState::WaitingKeyframe).await.is_err() {
                            return Outcome::Shutdown;
                        }
                    }
                    if !live && self.handle.stats.session_frames() > 0 {
                        live = true;
                        tracing::info!(
                            camera = %label,
                            resolucao = ?self.handle.stats.resolution(),
                            "conectado"
                        );
                        if self.emit_state(CameraState::Live).await.is_err() {
                            return Outcome::Shutdown;
                        }
                    }

                    let motion = self.handle.stats.motion_events();
                    if motion > seen_motion {
                        seen_motion = motion;
                        tracing::info!(camera = %label, "movimento detectado");
                        if self.emit(EventKind::Motion).await.is_err() {
                            return Outcome::Shutdown;
                        }
                    }

                    if self.service_recording(recording, label, live).await.is_err() {
                        return Outcome::Shutdown;
                    }
                }
            }
        }
    }

    /// Watchdog, em duas perguntas.
    ///
    /// **Está chegando dado do NVR?** Se não, a conexão morreu e reiniciamos
    /// rápido (`stall_timeout`).
    ///
    /// **Está saindo quadro?** Se não, mas os bytes continuam chegando, a
    /// pipeline não travou: está esperando um keyframe. Com `wait_for_keyframe`
    /// e um GOP longo isso acontece na conexão e depois de qualquer perda de
    /// pacote, e usar aqui o mesmo prazo curto do caso anterior derrubaria a
    /// pipeline antes de ela alcançar o próximo I-frame — em loop. Daí o prazo
    /// separado `keyframe_timeout` (que o chamador iguala a `stall_timeout`
    /// quando `wait_for_keyframe` está desligado).
    fn check_liveness(&self, live: bool) -> Option<Outcome> {
        let data_idle = self.handle.stats.data_idle_for();
        if data_idle > self.stall_timeout {
            return Some(Outcome::Lost {
                reason: format!("sem dados do NVR há {}s", data_idle.as_secs()),
                was_live: live,
            });
        }

        let frame_idle = self.handle.stats.idle_for();
        if frame_idle > self.keyframe_timeout {
            return Some(Outcome::Lost {
                reason: format!("sem quadro decodificado há {}s", frame_idle.as_secs()),
                was_live: live,
            });
        }
        None
    }

    /// Trata uma mensagem do bus. `Some(_)` encerra o ciclo atual.
    async fn handle_message(
        &self,
        message: &gst::Message,
        recording: &mut RecordingState,
        label: &str,
        live: bool,
    ) -> Option<Outcome> {
        // O `EOS` de um ramo de gravação que está sendo desligado.
        if let Some(stopping) = &recording.stopping
            && stopping.matches_eos(message)
        {
            self.finish_stopping(recording, label);
            return None;
        }

        match message.view() {
            MessageView::Error(err) => {
                let reason = self.redactor.apply(&err.error().to_string());
                let detail = err.debug().map(|d| self.redactor.apply(&d));

                // Um erro dentro do ramo de gravação (disco cheio, por
                // exemplo) não pode derrubar a visualização ao vivo.
                // O ramo pode já ter migrado para `stopping` quando o erro
                // chega (o EOS da parada é justamente o que costuma revelar um
                // problema no ramo), então checamos os dois.
                let from_recording = message.src().is_some_and(|src| {
                    recording
                        .active
                        .iter()
                        .chain(recording.stopping.iter())
                        .any(|branch| branch.owns(src))
                });
                if from_recording {
                    tracing::error!(camera = %label, erro = %reason, detalhe = ?detail, "erro na gravação");
                    recording.wanted = false;
                    let _ = self
                        .emit(EventKind::Recording(RecordingStatus::Failed(reason)))
                        .await;
                    return None;
                }

                tracing::error!(camera = %label, erro = %reason, detalhe = ?detail, "erro na pipeline");
                Some(Outcome::Lost {
                    reason,
                    was_live: live,
                })
            }
            MessageView::Eos(_) => Some(Outcome::Lost {
                reason: "fim de stream".into(),
                was_live: live,
            }),
            MessageView::Warning(warn) => {
                tracing::warn!(
                    camera = %label,
                    aviso = %self.redactor.apply(&warn.error().to_string()),
                    "aviso da pipeline"
                );
                None
            }
            _ => None,
        }
    }

    // -- ciclo de vida da gravação ------------------------------------------

    /// Aproxima o estado real da gravação do estado desejado.
    async fn service_recording(
        &self,
        recording: &mut RecordingState,
        label: &str,
        live: bool,
    ) -> Result<(), ()> {
        // Ramo preso esperando um EOS que não veio.
        if let Some(stopping) = &recording.stopping
            && stopping.stop_timed_out()
        {
            tracing::warn!(camera = %label, "EOS da gravação não chegou; removendo o ramo assim mesmo");
            self.finish_stopping(recording, label);
        }

        if recording.wanted && recording.active.is_none() && recording.stopping.is_none() && live {
            match recording::start(
                &self.handle.pipeline,
                self.handle.recording_tee(),
                &self.camera,
                &self.recording_options,
            ) {
                Ok(started) => {
                    let pattern = started.pattern.clone();
                    recording.active = Some(started);
                    self.emit(EventKind::Recording(RecordingStatus::Started { pattern }))
                        .await?;
                }
                Err(err) => {
                    let reason = format!("{err:#}");
                    tracing::error!(camera = %label, erro = %reason, "falha ao iniciar a gravação");
                    recording.wanted = false;
                    self.emit(EventKind::Recording(RecordingStatus::Failed(reason)))
                        .await?;
                }
            }
        }

        if !recording.wanted
            && let Some(mut active) = recording.active.take()
        {
            tracing::info!(
                camera = %label,
                duracao_s = active.elapsed().as_secs(),
                "encerrando gravação"
            );
            active.begin_stop(self.handle.recording_tee());
            recording.stopping = Some(active);
        }

        Ok(())
    }

    /// Remove da pipeline o ramo que terminou de fechar o arquivo.
    fn finish_stopping(&self, recording: &mut RecordingState, label: &str) {
        let Some(stopping) = recording.stopping.take() else {
            return;
        };
        let pattern = stopping.pattern.clone();
        match stopping.finish(&self.handle.pipeline) {
            Ok(()) => tracing::info!(camera = %label, arquivos = %pattern, "gravação finalizada"),
            Err(err) => {
                tracing::error!(camera = %label, erro = %format!("{err:#}"), "falha ao remover o ramo de gravação")
            }
        }
        // Notificar a UI é responsabilidade de quem pediu a parada; aqui só
        // avisamos que o arquivo fechou.
        let _ = self.events.try_send(CameraEvent {
            camera_id: self.camera.id,
            kind: EventKind::Recording(RecordingStatus::Stopped),
        });
    }

    /// Fecha a gravação antes de a pipeline ir para `NULL`.
    ///
    /// `wanted` é preservado: se a queda foi temporária, a gravação recomeça
    /// (em arquivo novo) assim que o vídeo voltar.
    async fn teardown_recording(
        &self,
        messages: &mut (impl StreamExt<Item = gst::Message> + Unpin),
        recording: &mut RecordingState,
        label: &str,
    ) {
        if let Some(mut active) = recording.active.take() {
            active.begin_stop(self.handle.recording_tee());
            recording.stopping = Some(active);
        }
        if recording.stopping.is_none() {
            return;
        }

        // Continuamos bombeando o bus só para pegar o EOS do ramo; as demais
        // mensagens não importam, a pipeline vai para NULL logo em seguida.
        let deadline = tokio::time::Instant::now() + recording::STOP_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                tracing::warn!(camera = %label, "EOS da gravação não chegou a tempo");
                break;
            }
            let Ok(Some(message)) = tokio::time::timeout(remaining, messages.next()).await else {
                break;
            };
            if recording
                .stopping
                .as_ref()
                .is_some_and(|stopping| stopping.matches_eos(&message))
            {
                break;
            }
        }
        self.finish_stopping(recording, label);
    }

    // -- comunicação com a UI -----------------------------------------------

    async fn emit_state(&self, state: CameraState) -> Result<(), ()> {
        self.emit(EventKind::State(state)).await
    }

    /// Envia um evento para a UI. `Err` significa que a UI sumiu (app fechando).
    async fn emit(&self, kind: EventKind) -> Result<(), ()> {
        self.events
            .send(CameraEvent {
                camera_id: self.camera.id,
                kind,
            })
            .await
            .map_err(|_| ())
    }

    /// `false` se o app encerrou durante a espera.
    async fn sleep_or_shutdown(&self, delay: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(delay) => true,
            _ = self.shutdown.recv() => false,
            // A UI largou o canal de comandos: a câmera foi removida.
            _ = commands_closed(&self.commands) => false,
        }
    }
}

/// Resolve quando ninguém mais pode mandar comandos a este supervisor.
async fn commands_closed(commands: &async_channel::Receiver<Command>) {
    while !commands.is_closed() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Health-check de rede: o NVR aceita conexão TCP na porta RTSP?
///
/// Distingue "câmera com problema" de "NVR/rede fora do ar", o que muda a
/// mensagem mostrada no tile.
pub async fn probe_tcp(host: &str, port: u16, timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, tokio::net::TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_dobra_ate_o_teto() {
        let backoff = Backoff::new(2, 30);
        assert_eq!(backoff.delay(0), Duration::from_secs(2));
        assert_eq!(backoff.delay(1), Duration::from_secs(4));
        assert_eq!(backoff.delay(2), Duration::from_secs(8));
        assert_eq!(backoff.delay(3), Duration::from_secs(16));
        assert_eq!(
            backoff.delay(4),
            Duration::from_secs(30),
            "deve saturar no teto"
        );
        assert_eq!(backoff.delay(99), Duration::from_secs(30), "sem overflow");
    }

    #[test]
    fn backoff_normaliza_valores_degenerados() {
        // initial = 0 vira 1 s; max abaixo do initial vira o próprio initial.
        let backoff = Backoff::new(0, 0);
        assert_eq!(backoff.delay(0), Duration::from_secs(1));
        assert_eq!(backoff.delay(10), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn probe_falha_em_porta_fechada() {
        // Porta 1 em loopback: recusa imediata, sem depender da rede.
        assert!(!probe_tcp("127.0.0.1", 1, Duration::from_millis(500)).await);
    }
}

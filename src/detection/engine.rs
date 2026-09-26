//! Motor de inferência: prepara o modelo (baixa, verifica, carrega) e roda as
//! análises numa fila compartilhada por todas as câmeras.
//!
//! Uma [`Engine`] existe por processo. O modelo só é preparado quando a
//! primeira câmera com detecção sobe, então quem não usa o recurso não paga
//! nada — nem download, nem memória.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::DetectionState;
use super::yolo::{DevicePref, Model, Params};
use crate::config;

/// YOLOv8n (COCO, fp32, ONNX) fixado por commit: o conteúdo não muda debaixo
/// dos nossos pés e o hash abaixo o confirma.
pub const DEFAULT_MODEL_URL: &str = "https://huggingface.co/kshitijjjjjjjjjjjjjjjj/yolov8n-coco-onnx/\
     resolve/7fdfbb82e54ed6b3c70722884f563450622b30fd/yolov8n.onnx";
pub const DEFAULT_MODEL_SHA256: &str =
    "013a98f3bc0264a3d793ef29ccbd178ceb0bbb86bc12ff3510273ce85b1c4526";

/// Teto do download: o YOLOv8n tem ~12 MB; isto só barra um servidor que
/// resolva mandar um fluxo sem fim.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Em que pé está o modelo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Ninguém pediu ainda.
    Idle,
    Downloading {
        /// `None` quando o servidor não informa o tamanho.
        percent: Option<u8>,
    },
    Loading,
    Ready,
    Failed(String),
}

impl Status {
    /// Texto curto para mostrar sobre o vídeo; `None` quando está tudo bem.
    pub fn describe(&self) -> Option<String> {
        match self {
            Status::Idle | Status::Ready => None,
            Status::Downloading { percent: Some(p) } => {
                Some(format!("IA: baixando o modelo… {p}%"))
            }
            Status::Downloading { percent: None } => Some("IA: baixando o modelo…".to_string()),
            Status::Loading => Some("IA: carregando o modelo…".to_string()),
            Status::Failed(reason) => Some(format!("IA indisponível: {reason}")),
        }
    }
}

struct Job {
    rgb: Vec<u8>,
    width: usize,
    height: usize,
    state: Arc<DetectionState>,
}

pub struct Engine {
    config: config::Detection,
    status: Mutex<Status>,
    /// Dispositivo em que o modelo está rodando, depois de carregado.
    device: Mutex<Option<String>>,
    /// Existe a partir do momento em que o modelo está pronto.
    jobs: OnceLock<mpsc::Sender<Job>>,
    /// Já tem um carregamento em andamento?
    preparing: AtomicBool,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("status", &*self.status.lock().unwrap())
            .finish_non_exhaustive()
    }
}

impl Engine {
    pub fn new(config: config::Detection) -> Self {
        Self {
            config,
            status: Mutex::new(Status::Idle),
            device: Mutex::new(None),
            jobs: OnceLock::new(),
            preparing: AtomicBool::new(false),
        }
    }

    /// `"CPU"`, `"NPU (Intel(R) AI Boost)"`… — `None` até o modelo carregar.
    pub fn device(&self) -> Option<String> {
        self.device.lock().unwrap().clone()
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    fn set_status(&self, status: Status) {
        *self.status.lock().unwrap() = status;
    }

    /// Começa (em segundo plano) a preparar o modelo. Idempotente: com o modelo
    /// pronto ou já em preparo não faz nada; depois de uma falha de carga,
    /// tenta de novo — é o que acontece ao religar a detecção numa câmera.
    pub fn prepare(self: &Arc<Self>) {
        if self.jobs.get().is_some() || self.preparing.swap(true, Ordering::AcqRel) {
            return;
        }
        let engine = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("detection-loader".into())
            .spawn(move || {
                match engine.load() {
                    Ok(()) => tracing::info!("modelo de detecção pronto"),
                    Err(err) => {
                        let reason = format!("{err:#}");
                        tracing::error!(erro = %reason, "modelo de detecção indisponível");
                        engine.set_status(Status::Failed(reason));
                    }
                }
                engine.preparing.store(false, Ordering::Release);
            });
        if let Err(err) = spawned {
            self.set_status(Status::Failed(format!(
                "não consegui criar a thread: {err}"
            )));
            self.preparing.store(false, Ordering::Release);
        }
    }

    fn load(self: &Arc<Self>) -> Result<()> {
        let path = self.config.model_file();
        if !path.is_file() {
            let url = self
                .config
                .model_url
                .as_deref()
                .unwrap_or(DEFAULT_MODEL_URL);
            let sha = self
                .config
                .model_sha256
                .as_deref()
                .or_else(|| (self.config.model_url.is_none()).then_some(DEFAULT_MODEL_SHA256));
            self.set_status(Status::Downloading { percent: Some(0) });
            self.download(url, sha, &path)?;
        }

        self.set_status(Status::Loading);
        let pref = DevicePref::parse(&self.config.device).unwrap_or(DevicePref::Auto);
        let model = Arc::new(
            Model::load(&path, self.config.input_size, pref)
                .with_context(|| format!("o modelo {} não é utilizável", path.display()))?,
        );
        tracing::info!(
            dispositivo = model.device(),
            "identificação de objetos pronta"
        );
        *self.device.lock().unwrap() = Some(model.device().to_string());

        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for index in 0..self.config.workers {
            let (engine, model, rx) = (Arc::clone(self), Arc::clone(&model), Arc::clone(&rx));
            std::thread::Builder::new()
                .name(format!("detection-{index}"))
                .spawn(move || engine.work(&model, &rx))
                .context("não consegui criar a thread de inferência")?;
        }
        let _ = self.jobs.set(tx);
        self.set_status(Status::Ready);
        Ok(())
    }

    fn work(&self, model: &Model, rx: &Mutex<mpsc::Receiver<Job>>) {
        // Cada thread tem o seu contexto de execução (no OpenVINO, uma
        // requisição de inferência própria).
        let mut session = match model.session() {
            Ok(session) => session,
            Err(err) => {
                self.set_status(Status::Failed(format!("{err:#}")));
                return;
            }
        };
        loop {
            // O lock só segura a espera; quem pega o job já o solta para os
            // outros workers poderem esperar o próximo.
            let job = match rx.lock().unwrap().recv() {
                Ok(job) => job,
                Err(_) => return,
            };
            let (mask, min_confidence) = job.state.active();
            let params = Params {
                classes: &mask,
                min_confidence,
                iou: self.config.iou,
            };
            match session.detect(&job.rgb, job.width, job.height, &params) {
                Ok(found) => job.state.publish(found, job.width, job.height),
                Err(err) => {
                    // Modelo que falha em quadro válido não vai melhorar sozinho.
                    let reason = format!("{err:#}");
                    tracing::error!(erro = %reason, "falha na inferência");
                    self.set_status(Status::Failed(reason));
                    job.state.finish();
                }
            }
        }
    }

    /// Entrega um quadro RGB para análise. Sem modelo pronto, o quadro é
    /// descartado e a vaga da câmera é devolvida.
    pub fn submit(&self, rgb: Vec<u8>, width: usize, height: usize, state: &Arc<DetectionState>) {
        let job = Job {
            rgb,
            width,
            height,
            state: Arc::clone(state),
        };
        let sent = self.status() == Status::Ready
            && self.jobs.get().is_some_and(|tx| tx.send(job).is_ok());
        if !sent {
            state.finish();
        }
    }

    fn download(&self, url: &str, sha256: Option<&str>, dest: &Path) -> Result<()> {
        tracing::info!(url, destino = %dest.display(), "baixando o modelo de detecção");
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("não consegui criar {}", dir.display()))?;
        }

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(600)))
            .build()
            .into();
        let mut response = agent
            .get(url)
            .call()
            .with_context(|| format!("falha ao baixar {url}"))?;
        let total = response.body().content_length();
        let mut reader = response
            .body_mut()
            .with_config()
            .limit(MAX_DOWNLOAD_BYTES)
            .reader();

        // Grava num `.part` e só renomeia depois de verificado: uma queda no
        // meio nunca deixa um modelo truncado no lugar do bom.
        let part = dest.with_extension("onnx.part");
        let result = (|| -> Result<()> {
            let mut file = fs::File::create(&part)
                .with_context(|| format!("não consegui criar {}", part.display()))?;
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            let mut received = 0u64;
            let mut last_percent = 0u8;
            loop {
                let n = reader.read(&mut buffer).context("download interrompido")?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
                file.write_all(&buffer[..n])?;
                received += n as u64;
                if let Some(total) = total.filter(|&t| t > 0) {
                    let percent = (received * 100 / total).min(100) as u8;
                    if percent != last_percent {
                        last_percent = percent;
                        self.set_status(Status::Downloading {
                            percent: Some(percent),
                        });
                    }
                }
            }
            file.flush()?;

            if let Some(expected) = sha256 {
                let actual = hex(&hasher.finalize());
                if !actual.eq_ignore_ascii_case(expected) {
                    bail!("o SHA-256 do download não confere (esperado {expected}, veio {actual})");
                }
            }
            fs::rename(&part, dest)
                .with_context(|| format!("não consegui gravar {}", dest.display()))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&part);
        }
        result
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_em_minusculas() {
        assert_eq!(hex(&[0x01, 0xab, 0xff]), "01abff");
    }

    #[test]
    fn hash_padrao_tem_formato_valido() {
        assert_eq!(DEFAULT_MODEL_SHA256.len(), 64);
        assert!(DEFAULT_MODEL_SHA256.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn so_o_estado_pronto_fica_em_silencio() {
        assert_eq!(Status::Ready.describe(), None);
        assert_eq!(Status::Idle.describe(), None);
        assert!(Status::Loading.describe().is_some());
        assert!(
            Status::Downloading { percent: Some(42) }
                .describe()
                .unwrap()
                .contains("42%")
        );
        assert!(
            Status::Failed("sem rede".into())
                .describe()
                .unwrap()
                .contains("sem rede")
        );
    }

    #[test]
    fn sem_modelo_pronto_o_quadro_e_descartado_e_a_vaga_volta() {
        let engine = Arc::new(Engine::new(config::Detection::default()));
        let state = Arc::new(DetectionState::new(
            Arc::clone(&engine),
            &super::super::DetectionSettings::default(),
            0.45,
            Duration::from_secs(1),
            Duration::from_secs(1),
        ));
        assert!(state.try_begin());
        engine.submit(vec![0; 12], 2, 2, &state);
        assert!(state.try_begin(), "a vaga precisa ter sido devolvida");
    }
}

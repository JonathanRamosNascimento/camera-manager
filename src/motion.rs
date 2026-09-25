//! Detecção de movimento por diferença de quadros.
//!
//! O `motioncells` do `gst-plugins-bad` depende de OpenCV e não está disponível
//! em toda instalação, então fazemos o diff aqui mesmo: a pipeline deriva uma
//! cópia minúscula em GRAY8 (ver `pipeline::attach_motion_branch`) e cada quadro
//! é comparado com o anterior. Um pixel conta como "alterado" quando difere
//! mais que `threshold`; se a fração de pixels alterados passa de `sensitivity`,
//! é movimento.
//!
//! Em GRAY8 com largura múltipla de 4 não há padding de linha, então o buffer
//! pode ser comparado como um vetor contínuo.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct MotionDetector {
    /// Diferença mínima por pixel (0–255) para contá-lo como alterado.
    threshold: u8,
    /// Fração da imagem que precisa mudar para disparar.
    sensitivity: f64,
    /// Tempo mínimo entre dois disparos.
    cooldown: Duration,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    previous: Vec<u8>,
    last_trigger: Option<Instant>,
}

impl MotionDetector {
    pub fn new(config: &crate::config::Motion) -> Self {
        Self::with_params(
            config.threshold,
            config.sensitivity,
            Duration::from_secs(config.cooldown_secs),
        )
    }

    pub fn with_params(threshold: u8, sensitivity: f64, cooldown: Duration) -> Self {
        Self {
            threshold,
            sensitivity,
            cooldown,
            state: Mutex::new(State::default()),
        }
    }

    /// Alimenta um quadro GRAY8 e diz se houve movimento.
    ///
    /// O primeiro quadro (ou qualquer mudança de resolução) só serve de
    /// referência e nunca dispara.
    pub fn feed(&self, frame: &[u8]) -> bool {
        if frame.is_empty() {
            return false;
        }
        let mut state = self.state.lock().unwrap();

        if state.previous.len() != frame.len() {
            state.previous.clear();
            state.previous.extend_from_slice(frame);
            return false;
        }

        let changed = frame
            .iter()
            .zip(state.previous.iter())
            .filter(|&(&current, &previous)| current.abs_diff(previous) >= self.threshold)
            .count();
        state.previous.copy_from_slice(frame);

        if (changed as f64 / frame.len() as f64) < self.sensitivity {
            return false;
        }

        // O cooldown evita uma enxurrada de eventos enquanto alguém atravessa
        // o quadro; queremos "houve movimento", não um evento por quadro.
        let now = Instant::now();
        match state.last_trigger {
            Some(last) if now.duration_since(last) < self.cooldown => false,
            _ => {
                state.last_trigger = Some(now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> MotionDetector {
        MotionDetector::with_params(24, 0.10, Duration::from_millis(50))
    }

    #[test]
    fn primeiro_quadro_e_apenas_referencia() {
        let detector = detector();
        assert!(!detector.feed(&[0u8; 100]));
    }

    #[test]
    fn quadros_iguais_nao_disparam() {
        let detector = detector();
        detector.feed(&[10u8; 100]);
        assert!(!detector.feed(&[10u8; 100]));
    }

    #[test]
    fn ruido_abaixo_do_threshold_nao_dispara() {
        let detector = detector();
        detector.feed(&[100u8; 100]);
        // Metade dos pixels muda, mas só 10 níveis — abaixo do threshold de 24.
        let mut frame = [100u8; 100];
        frame[..50].fill(110);
        assert!(!detector.feed(&frame));
    }

    #[test]
    fn area_pequena_alterada_nao_dispara() {
        let detector = detector();
        detector.feed(&[0u8; 100]);
        // 5 % da imagem estourada de brilho, abaixo da sensibilidade de 10 %.
        let mut frame = [0u8; 100];
        frame[..5].fill(255);
        assert!(!detector.feed(&frame));
    }

    #[test]
    fn mudanca_ampla_dispara() {
        let detector = detector();
        detector.feed(&[0u8; 100]);
        assert!(detector.feed(&[255u8; 100]));
    }

    #[test]
    fn cooldown_segura_o_segundo_disparo() {
        let detector = detector();
        detector.feed(&[0u8; 100]);
        assert!(detector.feed(&[255u8; 100]), "primeiro disparo");
        assert!(!detector.feed(&[0u8; 100]), "ainda dentro do cooldown");

        std::thread::sleep(Duration::from_millis(60));
        assert!(detector.feed(&[255u8; 100]), "cooldown expirou");
    }

    #[test]
    fn mudanca_de_resolucao_reinicia_a_referencia() {
        let detector = detector();
        detector.feed(&[0u8; 100]);
        assert!(!detector.feed(&[255u8; 64]), "tamanho novo é só referência");
        assert!(!detector.feed(&[255u8; 64]), "igual à nova referência");
    }
}

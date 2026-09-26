//! Inferência de modelos YOLO (v8/v11 exportados para ONNX) com o `tract`.
//!
//! `tract` é Rust puro: não traz biblioteca nativa nenhuma, então os
//! instaladores de cada plataforma continuam sendo só o app.
//!
//! O modelo recebe um quadro RGB, devolve uma grade de candidatos e daí sai a
//! lista de [`Detection`]. As etapas — *letterbox*, decodificação e NMS — são
//! funções puras, testadas sem precisar do arquivo `.onnx`.
//!
//! Layouts de saída aceitos: `[1, 4+80, N]` (o padrão do Ultralytics v8/v11) e
//! `[1, N, 4+80]` (exportações transpostas). O YOLOv5 (com *objectness*) e os
//! modelos que já trazem NMS embutido não são suportados.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tract_onnx::prelude::*;

use super::{Detection, classes, ov};

/// Cor de preenchimento do letterbox (a mesma do treino do Ultralytics).
const PAD_VALUE: f32 = 114.0 / 255.0;

type Plan = Arc<TypedRunnableModel>;

/// Onde o usuário quer rodar o modelo (`[detection].device`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicePref {
    /// NPU se houver (e o OpenVINO estiver instalado); senão CPU.
    Auto,
    Cpu,
    Npu,
    Gpu,
}

impl DevicePref {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "cpu" => Some(Self::Cpu),
            "npu" => Some(Self::Npu),
            "gpu" => Some(Self::Gpu),
            _ => None,
        }
    }
}

enum Runner {
    Tract(Plan),
    OpenVino(ov::Backend),
}

/// Modelo carregado, compartilhado pelas threads de inferência. Cada thread
/// pega o seu [`Session`].
pub struct Model {
    runner: Runner,
    size: usize,
    device: String,
}

/// O que a decodificação precisa saber além do tensor.
pub struct Params<'a> {
    /// `true` nas classes que interessam.
    pub classes: &'a [bool; classes::COUNT],
    /// Confiança mínima (0.0–1.0).
    pub min_confidence: f32,
    /// Sobreposição (IoU) acima da qual duas caixas da mesma classe são a
    /// mesma detecção.
    pub iou: f32,
}

impl Model {
    /// Carrega o modelo para a entrada quadrada `size`×`size`.
    ///
    /// Com `Auto`, `Npu` ou `Gpu` tenta primeiro o OpenVINO e só o aceita se
    /// um quadro de teste passar de ponta a ponta nele; qualquer falha (sem
    /// biblioteca, sem dispositivo, modelo que o dispositivo não compila) cai
    /// na CPU. Só `Auto` trata isso como normal: nos outros o motivo vai para o
    /// log como aviso, já que o usuário pediu aquele dispositivo.
    pub fn load(path: &Path, size: usize, pref: DevicePref) -> Result<Self> {
        if pref != DevicePref::Cpu {
            match Self::load_openvino(path, size, pref) {
                Ok(model) => return Ok(model),
                // Sem NPU na máquina, cair na CPU é o normal; com NPU, o usuário
                // quer saber por que ela não foi usada.
                Err(err) if pref == DevicePref::Auto && !ov::npu_present() => {
                    tracing::info!(motivo = %format!("{err:#}"), "sem aceleração por NPU; usando a CPU");
                }
                Err(err) if pref == DevicePref::Auto => {
                    tracing::warn!(motivo = %format!("{err:#}"), "há uma NPU, mas não consegui usá-la; usando a CPU");
                }
                Err(err) => {
                    tracing::warn!(motivo = %format!("{err:#}"), "não consegui usar o dispositivo pedido; usando a CPU");
                }
            }
        }
        let model = Self::load_tract(path, size)?;
        model.warm_up()?;
        Ok(model)
    }

    fn load_openvino(path: &Path, size: usize, pref: DevicePref) -> Result<Self> {
        let backend = ov::Backend::load(path, size, pref)?;
        let device = backend.label().to_string();
        let model = Self {
            runner: Runner::OpenVino(backend),
            size,
            device,
        };
        model
            .warm_up()
            .with_context(|| format!("o modelo não rodou em {}", model.device))?;
        Ok(model)
    }

    fn load_tract(path: &Path, size: usize) -> Result<Self> {
        let plan = tract_onnx::onnx()
            .model_for_path(path)
            .with_context(|| format!("não consegui ler o modelo {}", path.display()))?
            .with_input_fact(0, f32::fact([1, 3, size, size]).into())
            .with_context(|| format!("o modelo não aceita entrada {size}×{size}"))?
            .into_optimized()
            .context("não consegui otimizar o modelo")?
            .into_runnable()
            .context("não consegui preparar o modelo")?;
        Ok(Self {
            runner: Runner::Tract(plan),
            size,
            device: "CPU".to_string(),
        })
    }

    /// Nome do dispositivo em uso (`"CPU"`, `"NPU (Intel(R) AI Boost)"`…).
    pub fn device(&self) -> &str {
        &self.device
    }

    /// Um quadro preto de ponta a ponta: pega na hora um modelo com saída
    /// incompatível, em vez de falhar só quando a primeira câmera mandar um quadro.
    fn warm_up(&self) -> Result<()> {
        let (width, height) = (self.size, self.size * 9 / 16);
        self.session()?
            .detect(
                &vec![0u8; width * height * 3],
                width,
                height,
                &Params {
                    classes: &[true; classes::COUNT],
                    min_confidence: 0.99,
                    iou: 0.45,
                },
            )
            .map(drop)
    }

    /// Contexto de execução de uma thread.
    pub fn session(&self) -> Result<Session> {
        let inner = match &self.runner {
            Runner::Tract(plan) => SessionInner::Tract(Arc::clone(plan)),
            Runner::OpenVino(backend) => SessionInner::OpenVino(backend.session()?),
        };
        Ok(Session {
            inner,
            size: self.size,
        })
    }
}

pub struct Session {
    inner: SessionInner,
    size: usize,
}

enum SessionInner {
    Tract(Plan),
    OpenVino(ov::Session),
}

impl Session {
    /// Detecta objetos num quadro RGB (`width * height * 3` bytes, sem padding).
    pub fn detect(
        &mut self,
        rgb: &[u8],
        width: usize,
        height: usize,
        params: &Params,
    ) -> Result<Vec<Detection>> {
        let (input, lb) = letterbox(rgb, width, height, self.size);
        let (shape, data) = self.run(input)?;

        let (channels, anchors, channel_major) = match shape.as_slice() {
            [1, a, b] if a < b => (*a, *b, true),
            [1, a, b] => (*b, *a, false),
            other => bail!("saída do modelo com forma {other:?}; esperado [1, 84, N]"),
        };
        if channels != 4 + classes::COUNT {
            bail!(
                "o modelo tem {} classes; só modelos COCO (80 classes) são suportados",
                channels.saturating_sub(4)
            );
        }

        let candidates = decode(&data, channels, anchors, channel_major, &lb, params);
        Ok(nms(candidates, params.iou))
    }

    /// Roda a rede: devolve a forma e os dados da saída.
    fn run(&mut self, input: Vec<f32>) -> Result<(Vec<usize>, Vec<f32>)> {
        match &mut self.inner {
            SessionInner::Tract(plan) => {
                let size = self.size;
                let tensor = tract_ndarray::Array4::from_shape_vec((1, 3, size, size), input)
                    .context("tensor de entrada com forma inesperada")?;
                let outputs = plan
                    .run(tvec!(Tensor::from(tensor).into()))
                    .context("falha ao rodar o modelo")?;
                let output = outputs.first().context("o modelo não devolveu saída")?;
                let view = output
                    .to_plain_array_view::<f32>()
                    .context("saída do modelo não é f32")?;
                let data = view
                    .as_slice()
                    .context("saída do modelo não está contígua na memória")?
                    .to_vec();
                Ok((output.shape().to_vec(), data))
            }
            SessionInner::OpenVino(session) => session.run(&input),
        }
    }
}

// ---------------------------------------------------------------------------
// Pré-processamento
// ---------------------------------------------------------------------------

/// Como o quadro original foi encaixado no quadrado do modelo.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Letterbox {
    scale: f32,
    pad_x: f32,
    pad_y: f32,
    width: f32,
    height: f32,
}

/// Encaixa o quadro RGB num quadrado `size`×`size`, mantendo a proporção,
/// e devolve o tensor NCHW normalizado em 0–1.
///
/// Redimensiona por vizinho mais próximo: o pipeline já entrega o quadro na
/// largura do modelo, então na prática quase nunca há escala e o laço vira uma
/// cópia.
pub fn letterbox(rgb: &[u8], width: usize, height: usize, size: usize) -> (Vec<f32>, Letterbox) {
    debug_assert_eq!(rgb.len(), width * height * 3);
    let scale = (size as f32 / width as f32).min(size as f32 / height as f32);
    let new_w = ((width as f32 * scale).round() as usize).clamp(1, size);
    let new_h = ((height as f32 * scale).round() as usize).clamp(1, size);
    let pad_x = (size - new_w) / 2;
    let pad_y = (size - new_h) / 2;

    let plane = size * size;
    let mut out = vec![PAD_VALUE; 3 * plane];
    for dy in 0..new_h {
        let sy = ((dy as f32 / scale) as usize).min(height - 1);
        let row = &rgb[sy * width * 3..(sy + 1) * width * 3];
        for dx in 0..new_w {
            let sx = ((dx as f32 / scale) as usize).min(width - 1);
            let at = (pad_y + dy) * size + pad_x + dx;
            out[at] = f32::from(row[sx * 3]) / 255.0;
            out[plane + at] = f32::from(row[sx * 3 + 1]) / 255.0;
            out[2 * plane + at] = f32::from(row[sx * 3 + 2]) / 255.0;
        }
    }

    let letterbox = Letterbox {
        scale,
        pad_x: pad_x as f32,
        pad_y: pad_y as f32,
        width: width as f32,
        height: height as f32,
    };
    (out, letterbox)
}

// ---------------------------------------------------------------------------
// Pós-processamento
// ---------------------------------------------------------------------------

/// Converte a grade do modelo em candidatos, já filtrados por classe e
/// confiança e com as caixas de volta nas coordenadas (0–1) do quadro original.
fn decode(
    data: &[f32],
    channels: usize,
    anchors: usize,
    channel_major: bool,
    lb: &Letterbox,
    params: &Params,
) -> Vec<Detection> {
    let at = |channel: usize, anchor: usize| -> f32 {
        if channel_major {
            data[channel * anchors + anchor]
        } else {
            data[anchor * channels + channel]
        }
    };

    let mut found = Vec::new();
    for anchor in 0..anchors {
        let mut best: Option<(usize, f32)> = None;
        for class in (0..classes::COUNT).filter(|&c| params.classes[c]) {
            let score = at(4 + class, anchor);
            if score >= params.min_confidence && best.is_none_or(|(_, top)| score > top) {
                best = Some((class, score));
            }
        }
        let Some((class, score)) = best else { continue };

        let (cx, cy) = (at(0, anchor), at(1, anchor));
        let (bw, bh) = (at(2, anchor), at(3, anchor));
        if bw <= 0.0 || bh <= 0.0 {
            continue;
        }
        let to_x = |x: f32| ((x - lb.pad_x) / lb.scale / lb.width).clamp(0.0, 1.0);
        let to_y = |y: f32| ((y - lb.pad_y) / lb.scale / lb.height).clamp(0.0, 1.0);
        let detection = Detection {
            class,
            score,
            x1: to_x(cx - bw / 2.0),
            y1: to_y(cy - bh / 2.0),
            x2: to_x(cx + bw / 2.0),
            y2: to_y(cy + bh / 2.0),
        };
        // Caixa inteiramente no letterbox (fora da imagem): sem área útil.
        if detection.x2 > detection.x1 && detection.y2 > detection.y1 {
            found.push(detection);
        }
    }
    found
}

pub(super) fn iou(a: &Detection, b: &Detection) -> f32 {
    let w = (a.x2.min(b.x2) - a.x1.max(b.x1)).max(0.0);
    let h = (a.y2.min(b.y2) - a.y1.max(b.y1)).max(0.0);
    let inter = w * h;
    let union = a.area() + b.area() - inter;
    if union <= 0.0 { 0.0 } else { inter / union }
}

/// Supressão de não-máximos por classe: das caixas que cobrem o mesmo objeto,
/// fica a de maior confiança.
pub fn nms(mut candidates: Vec<Detection>, threshold: f32) -> Vec<Detection> {
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Detection> = Vec::new();
    for candidate in candidates {
        if kept
            .iter()
            .all(|k| k.class != candidate.class || iou(k, &candidate) < threshold)
        {
            kept.push(candidate);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_classes() -> [bool; classes::COUNT] {
        [true; classes::COUNT]
    }

    fn params(mask: &[bool; classes::COUNT], min_confidence: f32) -> Params<'_> {
        Params {
            classes: mask,
            min_confidence,
            iou: 0.45,
        }
    }

    fn det(class: usize, score: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> Detection {
        Detection {
            class,
            score,
            x1,
            y1,
            x2,
            y2,
        }
    }

    /// Monta a saída do modelo, `[4 + 80, anchors]`, com uma caixa por âncora.
    /// Cada item é `(cx, cy, w, h, classe, confiança)` em pixels do modelo.
    fn grid(boxes: &[(f32, f32, f32, f32, usize, f32)]) -> Vec<f32> {
        let n = boxes.len();
        let mut data = vec![0.0; (4 + classes::COUNT) * n];
        for (i, &(cx, cy, w, h, class, score)) in boxes.iter().enumerate() {
            data[i] = cx;
            data[n + i] = cy;
            data[2 * n + i] = w;
            data[3 * n + i] = h;
            data[(4 + class) * n + i] = score;
        }
        data
    }

    fn transpose(data: &[f32], channels: usize, anchors: usize) -> Vec<f32> {
        let mut out = vec![0.0; data.len()];
        for c in 0..channels {
            for a in 0..anchors {
                out[a * channels + c] = data[c * anchors + a];
            }
        }
        out
    }

    #[test]
    fn letterbox_paisagem_poe_faixas_em_cima_e_embaixo() {
        // 8×4 num quadrado de 8: sem escala, 2 linhas de faixa em cada lado.
        let rgb = vec![255u8; 8 * 4 * 3];
        let (tensor, lb) = letterbox(&rgb, 8, 4, 8);
        assert_eq!(tensor.len(), 3 * 64);
        assert_eq!((lb.scale, lb.pad_x, lb.pad_y), (1.0, 0.0, 2.0));
        assert_eq!(tensor[0], PAD_VALUE, "faixa de cima");
        assert_eq!(tensor[2 * 8], 1.0, "primeira linha da imagem");
        assert_eq!(tensor[5 * 8], 1.0, "última linha da imagem");
        assert_eq!(tensor[6 * 8], PAD_VALUE, "faixa de baixo");
    }

    #[test]
    fn letterbox_reduz_quadro_maior_que_o_modelo() {
        let rgb = vec![0u8; 16 * 8 * 3];
        let (_, lb) = letterbox(&rgb, 16, 8, 8);
        assert_eq!(lb.scale, 0.5);
        assert_eq!((lb.pad_x, lb.pad_y), (0.0, 2.0));
    }

    #[test]
    fn letterbox_separa_os_canais_em_planos() {
        let mut rgb = vec![0u8; 4 * 4 * 3];
        rgb[0] = 255; // primeiro pixel: só vermelho
        let (tensor, _) = letterbox(&rgb, 4, 4, 4);
        assert_eq!(tensor[0], 1.0);
        assert_eq!(tensor[16], 0.0);
        assert_eq!(tensor[32], 0.0);
    }

    #[test]
    fn decodifica_e_devolve_a_caixa_ao_quadro_original() {
        // Quadro 8×4 num modelo de 8: pad_y = 2, sem escala.
        let (_, lb) = letterbox(&[0u8; 8 * 4 * 3], 8, 4, 8);
        let data = grid(&[(4.0, 4.0, 4.0, 2.0, 16, 0.9)]); // cachorro no centro
        let mask = all_classes();
        let found = decode(&data, 84, 1, true, &lb, &params(&mask, 0.5));

        assert_eq!(found.len(), 1);
        let d = &found[0];
        assert_eq!(d.class, 16);
        assert!((d.x1 - 0.25).abs() < 1e-6 && (d.x2 - 0.75).abs() < 1e-6);
        assert!((d.y1 - 0.25).abs() < 1e-6 && (d.y2 - 0.75).abs() < 1e-6);
    }

    #[test]
    fn layout_transposto_da_o_mesmo_resultado() {
        let (_, lb) = letterbox(&[0u8; 8 * 4 * 3], 8, 4, 8);
        let data = grid(&[(4.0, 4.0, 4.0, 2.0, 0, 0.9), (2.0, 4.0, 2.0, 2.0, 2, 0.8)]);
        let mask = all_classes();
        let a = decode(&data, 84, 2, true, &lb, &params(&mask, 0.5));
        let b = decode(
            &transpose(&data, 84, 2),
            84,
            2,
            false,
            &lb,
            &params(&mask, 0.5),
        );
        assert_eq!(a, b);
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn ignora_classe_desmarcada_e_confianca_baixa() {
        let (_, lb) = letterbox(&[0u8; 8 * 4 * 3], 8, 4, 8);
        let data = grid(&[(4.0, 4.0, 4.0, 2.0, 0, 0.9), (4.0, 4.0, 4.0, 2.0, 2, 0.3)]);

        let mut only_cars = [false; classes::COUNT];
        only_cars[2] = true;
        let found = decode(&data, 84, 2, true, &lb, &params(&only_cars, 0.25));
        assert_eq!(found.len(), 1, "pessoa desmarcada");
        assert_eq!(found[0].class, 2);

        let found = decode(&data, 84, 2, true, &lb, &params(&only_cars, 0.5));
        assert!(found.is_empty(), "carro abaixo da confiança mínima");
    }

    #[test]
    fn caixa_toda_no_letterbox_e_descartada() {
        let (_, lb) = letterbox(&[0u8; 8 * 4 * 3], 8, 4, 8);
        // Centro em y=0.5, dentro da faixa de cima (que vai de 0 a 2).
        let data = grid(&[(4.0, 0.5, 2.0, 0.5, 0, 0.9)]);
        let mask = all_classes();
        assert!(decode(&data, 84, 1, true, &lb, &params(&mask, 0.5)).is_empty());
    }

    #[test]
    fn nms_mantem_a_de_maior_confianca_por_classe() {
        let kept = nms(
            vec![
                det(0, 0.6, 0.1, 0.1, 0.5, 0.5),
                det(0, 0.9, 0.12, 0.1, 0.5, 0.5),
                det(2, 0.7, 0.1, 0.1, 0.5, 0.5), // outra classe: não é suprimida
                det(0, 0.8, 0.6, 0.6, 0.9, 0.9), // longe: outra pessoa
            ],
            0.45,
        );
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].score, 0.9);
        assert!(kept.iter().all(|d| d.score != 0.6));
    }

    /// Teste manual com o modelo de verdade — serve para conferir em que
    /// dispositivo ele roda e quanto demora:
    ///
    /// ```sh
    /// gst-launch-1.0 -q filesrc location=rua.jpg ! jpegdec ! videoconvert ! videoscale ! \
    ///   video/x-raw,format=RGB,width=640,height=853,pixel-aspect-ratio=1/1 ! filesink location=rua.rgb
    /// YOLO_MODEL=yolov8n.onnx YOLO_FRAME=rua.rgb YOLO_SIZE=640x853 YOLO_DEVICE=auto \
    ///   cargo test --release dispositivo_real -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "precisa do modelo e de um quadro RGB (veja a documentação do teste)"]
    fn dispositivo_real() {
        let path = std::env::var("YOLO_MODEL").expect("YOLO_MODEL");
        let frame = std::fs::read(std::env::var("YOLO_FRAME").expect("YOLO_FRAME")).unwrap();
        let (w, h) = std::env::var("YOLO_SIZE")
            .expect("YOLO_SIZE=LxA")
            .split_once('x')
            .map(|(w, h)| (w.parse::<usize>().unwrap(), h.parse::<usize>().unwrap()))
            .unwrap();
        let pref = DevicePref::parse(&std::env::var("YOLO_DEVICE").unwrap_or("auto".into()))
            .expect("YOLO_DEVICE = auto|cpu|npu|gpu");

        let t = std::time::Instant::now();
        let model = Model::load(Path::new(&path), 640, pref).unwrap();
        println!("dispositivo: {} (carga {:?})", model.device(), t.elapsed());

        let mask = all_classes();
        let mut session = model.session().unwrap();
        let mut found = Vec::new();
        let mut times = Vec::new();
        for _ in 0..10 {
            let t = std::time::Instant::now();
            found = session.detect(&frame, w, h, &params(&mask, 0.45)).unwrap();
            times.push(t.elapsed());
        }
        times.sort();
        println!("por quadro: mediana {:?}, mínimo {:?}", times[5], times[0]);
        for d in &found {
            println!("  {} {:.0}%", classes::pt(d.class), d.score * 100.0);
        }
        assert!(
            found.iter().filter(|d| d.class == 0).count() >= 2,
            "pessoas"
        );
        assert!(found.iter().any(|d| d.class == 5), "ônibus");
    }
}

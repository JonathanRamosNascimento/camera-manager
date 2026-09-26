//! Backend OpenVINO: roda o modelo na NPU (ou GPU) Intel.
//!
//! A biblioteca do OpenVINO é carregada **em tempo de execução** (`runtime-linking`):
//! o app compila e roda sem ela. Se não estiver instalada, se não houver o
//! dispositivo pedido ou se o modelo não compilar nele, [`Backend::load`] devolve
//! erro e quem chamou volta para a CPU (`tract`), como sempre foi.
//!
//! Sem a feature `openvino` este módulo vira um esqueleto que sempre recusa.

use std::path::Path;

use super::yolo::DevicePref;

/// Existe alguma NPU nesta máquina (Linux: `/dev/accel/accel*`)?
pub fn npu_present() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/dev/accel").is_ok_and(|mut entries| entries.next().is_some())
    }
    #[cfg(not(target_os = "linux"))]
    false
}

/// Se existe uma NPU na máquina mas o usuário não tem permissão de usá-la, diz o
/// que fazer. No Linux o dispositivo (`/dev/accel/accel*`) é do grupo `render`;
/// sem estar nele o OpenVINO só enxerga a CPU, sem dar nenhum erro.
pub fn npu_permission_hint() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let unreadable = std::fs::read_dir("/dev/accel")
            .ok()?
            .flatten()
            .find(|entry| std::fs::File::open(entry.path()).is_err())?;
        Some(format!(
            "há uma NPU ({}), mas sem permissão para usá-la: adicione seu usuário ao grupo \
             `render` (sudo usermod -aG render $USER) e entre de novo na sessão",
            unreadable.path().display()
        ))
    }
    #[cfg(not(target_os = "linux"))]
    None
}

/// Nomes dos dispositivos OpenVINO que existem nesta máquina (`["CPU", "NPU"]`),
/// ou vazio se a biblioteca não está instalada. Usado por `--check`.
pub fn available_devices() -> Vec<String> {
    imp::available_devices()
}

#[cfg(feature = "openvino")]
mod imp {
    use std::sync::Mutex;

    use anyhow::{Context, Result, anyhow, bail};
    use openvino::{
        CompiledModel, Core, DeviceType, ElementType, InferRequest, PartialShape, PropertyKey,
        Shape, Tensor,
    };

    use super::{DevicePref, Path};

    pub fn available_devices() -> Vec<String> {
        Core::new()
            .ok()
            .and_then(|core| {
                core.available_devices().ok().map(|devices| {
                    devices
                        .iter()
                        .map(|d| d.as_ref().to_string())
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default()
    }

    pub struct Backend {
        // O `Core` precisa viver tanto quanto o modelo compilado.
        _core: Mutex<Core>,
        compiled: Mutex<CompiledModel>,
        size: usize,
        label: String,
    }

    pub struct Session {
        request: InferRequest,
        input: Tensor,
        size: usize,
    }

    impl Backend {
        pub fn load(path: &Path, size: usize, pref: DevicePref) -> Result<Self> {
            let wanted = match pref {
                DevicePref::Auto | DevicePref::Npu => "NPU",
                DevicePref::Gpu => "GPU",
                DevicePref::Cpu => bail!("CPU não usa o OpenVINO"),
            };
            let mut core = Core::new().map_err(|err| {
                let hint = if super::npu_present() {
                    " — há uma NPU: instale o OpenVINO (Arch: sudo pacman -S openvino \
                         openvino-intel-npu-plugin)"
                } else {
                    ""
                };
                anyhow!("OpenVINO não está instalado ou não carregou ({err}){hint}")
            })?;
            let devices: Vec<String> = core
                .available_devices()
                .map_err(|err| anyhow!("{err}"))?
                .iter()
                .map(|d| d.as_ref().to_string())
                .collect();
            // "GPU" pode aparecer como "GPU.0", "GPU.1"…: vale o primeiro.
            let device = devices
                .iter()
                .find(|d| *d == wanted || d.starts_with(&format!("{wanted}.")))
                .with_context(|| {
                    let hint = if wanted == "NPU" {
                        super::npu_permission_hint()
                    } else {
                        None
                    };
                    format!(
                        "o OpenVINO não enxerga {wanted} (dispositivos: {}){}",
                        devices.join(", "),
                        hint.map(|h| format!(" — {h}")).unwrap_or_default()
                    )
                })?
                .clone();

            let path = path.to_str().context("caminho do modelo não é UTF-8")?;
            let mut model = core
                .read_model_from_file(path, "")
                .map_err(|err| anyhow!("o OpenVINO não leu o modelo: {err}"))?;
            // NPU exige formas fixas.
            if model.is_dynamic() {
                let shape = PartialShape::new_static(4, &[1, 3, size as i64, size as i64])
                    .map_err(|err| anyhow!("{err}"))?;
                model.reshape_single_input(&shape).map_err(|err| {
                    anyhow!("não consegui fixar a entrada em {size}×{size}: {err}")
                })?;
            }
            let compiled = core
                .compile_model(&model, DeviceType::from(device.as_str()))
                .map_err(|err| anyhow!("o {device} não compilou o modelo: {err}"))?;

            let name = core
                .get_property(
                    &DeviceType::from(device.as_str()),
                    &PropertyKey::DeviceFullName,
                )
                .ok()
                .filter(|name| !name.trim().is_empty());
            let label = match name {
                Some(name) => format!("{wanted} ({})", name.trim()),
                None => wanted.to_string(),
            };

            Ok(Self {
                _core: Mutex::new(core),
                compiled: Mutex::new(compiled),
                size,
                label,
            })
        }

        pub fn label(&self) -> &str {
            &self.label
        }

        pub fn session(&self) -> Result<Session> {
            let mut request = self
                .compiled
                .lock()
                .unwrap()
                .create_infer_request()
                .map_err(|err| anyhow!("não consegui criar a requisição de inferência: {err}"))?;
            let size = self.size as i64;
            let shape = Shape::new(&[1, 3, size, size]).map_err(|err| anyhow!("{err}"))?;
            let input = Tensor::new(ElementType::F32, &shape).map_err(|err| anyhow!("{err}"))?;
            request
                .set_input_tensor(&input)
                .map_err(|err| anyhow!("{err}"))?;
            Ok(Session {
                request,
                input,
                size: self.size,
            })
        }
    }

    impl Session {
        /// Roda o modelo. Devolve a forma e os dados da saída.
        pub fn run(&mut self, input: &[f32]) -> Result<(Vec<usize>, Vec<f32>)> {
            debug_assert_eq!(input.len(), 3 * self.size * self.size);
            self.input
                .get_data_mut::<f32>()
                .map_err(|err| anyhow!("{err}"))?
                .copy_from_slice(input);
            self.request.infer().map_err(|err| anyhow!("{err}"))?;
            let output = self
                .request
                .get_output_tensor()
                .map_err(|err| anyhow!("{err}"))?;
            if output.get_element_type().map_err(|err| anyhow!("{err}"))? != ElementType::F32 {
                bail!("a saída do modelo não é f32");
            }
            let shape = output
                .get_shape()
                .map_err(|err| anyhow!("{err}"))?
                .get_dimensions()
                .iter()
                .map(|&d| d as usize)
                .collect();
            let data = output
                .get_data::<f32>()
                .map_err(|err| anyhow!("{err}"))?
                .to_vec();
            Ok((shape, data))
        }
    }
}

#[cfg(not(feature = "openvino"))]
mod imp {
    use anyhow::{Result, bail};

    use super::{DevicePref, Path};

    pub fn available_devices() -> Vec<String> {
        Vec::new()
    }

    pub struct Backend;
    pub struct Session;

    impl Backend {
        pub fn load(_: &Path, _: usize, _: DevicePref) -> Result<Self> {
            bail!("este build foi compilado sem a feature `openvino`")
        }
        pub fn label(&self) -> &str {
            ""
        }
        pub fn session(&self) -> Result<Session> {
            bail!("sem OpenVINO")
        }
    }

    impl Session {
        pub fn run(&mut self, _: &[f32]) -> Result<(Vec<usize>, Vec<f32>)> {
            bail!("sem OpenVINO")
        }
    }
}

pub use imp::{Backend, Session};

# nvr-dashboard

Dashboard nativo para Linux (Rust + GTK4 + GStreamer) que mostra ao vivo, num
grid, as câmeras de um NVR iCSee/XMEye (firmware Hi3520) ou câmeras IP via
RTSP — cadastradas pela própria janela, manualmente ou escaneando a rede — com
reconexão automática, captura de tela, gravação sob demanda, detecção de
movimento e ícone na bandeja.

![grid 2×2 com três câmeras](docs/screenshot.png)

_Captura do teste com credenciais inválidas: mostra os estados de conexão
(vermelho = reconectando com backoff), o layout 2×2 e os botões de captura e
gravação em cada tile._

---

## Sumário

- [Recursos](#recursos)
- [Requisitos](#requisitos)
- [Instalação](#instalação)
- [Configuração](#configuração)
- [Uso](#uso)
- [Arquitetura](#arquitetura)
- [Segurança](#segurança)
- [Problemas conhecidos](#problemas-conhecidos)
- [Diagnóstico](#diagnóstico)
- [Desenvolvimento](#desenvolvimento)

---

## Recursos

**Cadastro de câmeras**

- Abre **vazio** na primeira execução, com botões para escanear a rede ou adicionar manualmente
- Escaneia a sub-rede (porta RTSP 554 + ONVIF/WS-Discovery) e lista o que encontrar
- "Detectar canais" testa os canais 1–8 do dispositivo e marca só os que têm imagem
- Botão **Gerenciar câmeras** (barra de título): lista com dados ao vivo, editar, adicionar e remover, sem reiniciar o app

**Visualização**

- Grid adaptável: 1 câmera → 1×1, 2–4 → 2×2, 5–9 → 3×3 (ou colunas fixas por configuração)
- Uma pipeline GStreamer independente por câmera — uma câmera offline não afeta as outras
- Clique (ou tecla `1`–`9`) abre a câmera em tela cheia; `Esc` volta ao grid
- Status por tile: conectando / ao vivo / reconectando / falha, com resolução, fps e bitrate de rede

**Confiabilidade**

- Reconexão automática com backoff exponencial (2 s, 4 s, 8 s… até o teto), que zera quando o vídeo volta
- Watchdog de quadros: reinicia a pipeline se a câmera "congelar" sem emitir erro
- Health-check TCP do NVR, que distingue "câmera com problema" de "NVR fora do ar"
- Log estruturado de todos os eventos de pipeline

**Captura e gravação**

- Captura do quadro atual em PNG, na resolução nativa do vídeo
- Gravação sob demanda em MKV/MP4, **sem recodificar** e sem abrir uma segunda conexão RTSP
- Buffer circular via `splitmuxsink` (`segment_seconds` × `max_files`)

**Automação**

- Detecção de movimento por diferença de quadros, com sensibilidade e cooldown configuráveis
- Notificações do desktop quando uma câmera cai e quando volta
- Ícone na bandeja (StatusNotifierItem) com resumo e menu
- Cards com tamanho independente (arrastando borda/canto), reordenáveis (arrastando o card) e realocados automaticamente quando invadidos; layout salvo entre execuções
- Substream no grid e stream principal em tela cheia (`adaptive_stream`), para poupar CPU e banda
- Vários NVRs / câmeras IP no mesmo dashboard

---

## Requisitos

Tudo vem dos repositórios oficiais do Arch/CachyOS:

```sh
sudo pacman -S --needed rustup gtk4 gstreamer gst-plugins-base gst-plugins-good \
                        gst-plugins-bad gst-plugins-ugly gst-libav gst-plugin-gtk4
rustup default stable
```

| Componente | Para quê | Obrigatório |
|---|---|---|
| `gtk4` ≥ 4.12 | interface | sim |
| `gstreamer` + `plugins-base/good` | RTSP, decodificação, gravação | sim |
| `gst-plugin-gtk4` | elemento `gtk4paintablesink` | sim |
| `gst-libav` | decoders de software (`avdec_h264`, `avdec_h265`) | sim |
| `gst-plugin-va` / `nvcodec` | decodificação por hardware (opcional, ver [Problemas conhecidos](#problemas-conhecidos)) | não |
| Extensão GNOME AppIndicator | ícone na bandeja | não |

Verifique o ambiente sem compilar nada:

```sh
gst-inspect-1.0 gtk4paintablesink rtspsrc splitmuxsink parsebin >/dev/null && echo ok
```

---

## Instalação

**Rodando do diretório do projeto:**

```sh
cargo build --release
./target/release/nvr-dashboard
```

**Instalando no sistema** (binário, `.desktop` e ícone):

```sh
make                 # compila em release
sudo make install    # PREFIX=/usr/local por padrão
make user-config     # cria ~/.config/nvr-dashboard/cameras.toml (modo 600)
```

Depois disso o app aparece no menu do GNOME. Para remover: `sudo make uninstall`.

---

## Configuração

Há dois arquivos, com papéis diferentes:

| Arquivo | O que guarda | Quem edita |
|---|---|---|
| `~/.config/nvr-dashboard/devices.toml` | **Câmeras** e credenciais | O app (janela de cadastro) |
| `cameras.toml` | **Ajustes** do app (`[app]`, `[recording]`, `[motion]`…) | Você, opcional |

### Cadastrar câmeras (pela janela)

Na primeira execução o app abre sem câmeras. Use:

- **Escanear a rede** — varre a sub-rede da máquina (`/24`, editável) procurando a
  porta RTSP 554 aberta e câmeras ONVIF. Clique em **Adicionar…** no dispositivo
  encontrado, informe usuário e senha e use **Detectar canais**.
- **Adicionar manualmente** — endereço, porta, usuário, senha e canais (`1-3`,
  `1,2,5`…). Em **Avançado** dá para trocar o modelo da URL RTSP.
- **Gerenciar câmeras** (botão azul no canto esquerdo da barra de título) — abre a lista de
  câmeras cadastradas. Cada linha mostra o **nome**, o endereço (`host:porta`),
  o **canal**, o **usuário**, o estado ao vivo (ao vivo / conectando /
  reconectando…), resolução, fps e Mb/s, e a qualidade escolhida. Os dados
  atualizam a cada segundo. Dali você pode:
  - **Editar** — renomear a câmera e trocar endereço, porta, usuário, senha e o
    modelo da URL. Endereço, porta, usuário e senha valem para o **dispositivo
    inteiro** (todos os canais dele): as câmeras são recriadas mantendo posição,
    tamanho e qualidade no grid. Senha vazia = mantém a atual. Só o nome não
    interrompe o vídeo.
  - **Remover** — com confirmação; some do grid e do cadastro.
  - **Adicionar manualmente** / **Escanear a rede** — as duas opções acima.

**Detectar canais:** muitos NVRs aceitam a sessão RTSP até para canais que não
existem, então "conectou" não prova nada. O teste só considera um canal vivo se
chegar **vídeo** (até 8 s por canal). Um canal sem imagem pode ser câmera
offline, canal vazio, ou uma falha momentânea do NVR — se algum canal esperado
aparecer como "sem imagem", rode a detecção de novo.

Cada canal vira um card. Se o `host:porta` já existe, o app só acrescenta os
canais que faltam (e atualiza o login).

`devices.toml` é gravado com permissão `600` porque guarda as senhas em texto
(o mesmo cuidado que o `cameras.toml` sempre teve). Para outro caminho, use
`$NVR_DASHBOARD_DEVICES`. Se o arquivo ficar ilegível, ele é movido para
`devices.toml.bak` e o app abre vazio, em vez de sobrescrevê-lo.

### Ajustes (`cameras.toml`, opcional)

```sh
cp config/cameras.example.toml config/cameras.toml
chmod 600 config/cameras.toml
$EDITOR config/cameras.toml
```

`config/cameras.toml` está no `.gitignore`. O arquivo é procurado nesta ordem;
sem nenhum, valem os padrões:

1. `--config <ARQUIVO>`
2. `$NVR_DASHBOARD_CONFIG`
3. `./config/cameras.toml`
4. `$XDG_CONFIG_HOME/nvr-dashboard/cameras.toml`

> **Migração:** versões anteriores liam `[nvr]`, `[[nvrs]]` e `[[cameras]]` deste
> arquivo. Esses blocos ainda são aceitos (não quebram), mas **ignorados**, com um
> aviso no log: recadastre as câmeras pela janela e depois apague os blocos — eles
> guardam a senha do NVR em texto.

Todas as chaves estão comentadas em
[`config/cameras.example.toml`](config/cameras.example.toml). O modelo de URL
RTSP (campo **Avançado** do cadastro) aceita os placeholders `{host}` `{port}`
`{channel}` `{stream}` `{user}` `{password}` `{user_enc}` `{password_enc}`. Use as
variantes `_enc` na seção `usuario:senha@` — sem elas, uma senha com `@`, `/` ou
`:` quebra o parsing do endereço. O padrão (iCSee/XMEye) é:

```
rtsp://{user_enc}:{password_enc}@{host}:{port}/user={user}&password={password}&channel={channel}&stream={stream}.sdp
```

### `[app]` — comportamento

| Chave | Padrão | Descrição |
|---|---|---|
| `latency_ms` | `200` | Buffer de jitter do `rtspsrc` |
| `rtsp_protocols` | `"tcp"` | `tcp`, `udp` ou `tcp+udp` |
| `hardware_decoding` | `false` | Ver [Problemas conhecidos](#problemas-conhecidos) |
| `grid_columns` | automático | Colunas fixas do grid |
| `stall_timeout_secs` | `12` | Sem dados do NVR por este tempo → reinicia |
| `wait_for_keyframe` | `true` | Segura a exibição até o primeiro keyframe |
| `keyframe_timeout_secs` | `90` | Teto para "dados chegando, sem quadro" |
| `reconnect_initial_secs` | `2` | Primeiro intervalo do backoff |
| `reconnect_max_secs` | `60` | Teto do backoff |
| `adaptive_stream` | `false` (recomendado `true` com 3+ câmeras) | Substream no grid, principal em tela cheia |
| `substream_index` | `1` | Índice usado como substream |

### `[snapshots]`, `[recording]`, `[motion]`, `[notifications]`

| Seção | Chave | Padrão | Descrição |
|---|---|---|---|
| `snapshots` | `directory` | `<Imagens>/nvr-dashboard` | Onde salvar os PNG |
| `recording` | `directory` | `<Vídeos>/nvr-dashboard` | Onde salvar os vídeos |
| `recording` | `segment_seconds` | `300` | Duração de cada arquivo |
| `recording` | `max_files` | `0` | Arquivos mantidos por sessão (`0` = sem limite) |
| `recording` | `container` | `"mkv"` | `mkv` (robusto) ou `mp4` (portátil) |
| `motion` | `enabled` | `false` | Liga o ramo de análise |
| `motion` | `threshold` | `24` | Diferença mínima por pixel (0–255) |
| `motion` | `sensitivity` | `0.02` | Fração da imagem que precisa mudar |
| `motion` | `cooldown_secs` | `10` | Intervalo mínimo entre disparos |
| `motion` | `notify` | `false` | Notificação a cada detecção |
| `notifications` | `enabled` | `true` | Avisos de câmera offline/de volta |
| `notifications` | `offline_after_attempts` | `2` | Só avisa a partir desta tentativa |
| `notifications` | `tray` | `true` | Ícone na bandeja |

---

## Uso

```sh
cargo run --release                  # abre o dashboard
cargo run --release -- --check       # valida config + alcance dos dispositivos, sem GUI
cargo run --release -- --help
```

`--check` confirma que o TOML é válido, testa o TCP de cada dispositivo
cadastrado, lista as câmeras com a URL mascarada e mostra os diretórios de saída. É o primeiro
comando a rodar quando algo não funciona.

### Atalhos

| Tecla | Ação |
|---|---|
| Clique / `Enter` | Abre a câmera em foco em tela cheia |
| `1` … `9` | Abre a câmera daquela posição |
| `Esc` | Volta ao grid |
| `Ctrl+S` | Captura PNG da câmera em foco |
| `Ctrl+R` | Inicia/para a gravação da câmera em foco |
| `F11` | Alterna tela cheia da janela |
| `Ctrl+Q` / `Ctrl+W` | Sai |

Cada tile também tem botões de captura e gravação na faixa superior.

### Redimensionar e reorganizar os cards

O grid é feito de **células**, e cada card ocupa `largura × altura` células, com
tamanho **próprio**: mexer num card nunca altera o tamanho dos outros.

- **Redimensionar:** arraste a borda direita, a borda de baixo ou o canto
  (marcado no canto inferior direito) de um card. O tamanho encaixa nas células.
  Dá para esticar um card até o fim da janela.
- **Quem for invadido é realocado:** se o card cresce sobre outro, o invadido vai
  para o espaço livre mais próximo — de preferência para o lado (ex.: um card
  esticado até embaixo joga o que estava ali para a célula vazia ao lado). Sem
  espaço livre, ele encolhe o mínimo necessário; só em último caso desce para uma
  linha nova. Encolher o card de volta, na mesma tacada, devolve os outros.
- **Mover:** arraste um card e solte sobre **outro** (os dois trocam de lugar e
  de tamanho) ou sobre uma **célula vazia** (o card vai para lá).
- **Primeira abertura:** todos os cards têm o mesmo tamanho (`1×1`, preenchendo
  linha a linha); células que sobram ficam vazias.

Posição e tamanho de cada card são salvos ~0,5 s depois da mudança em
`$XDG_CONFIG_HOME/nvr-dashboard/layout.toml` (padrão
`~/.config/nvr-dashboard/layout.toml`), por câmera (`<dispositivo>/<canal>`).
Câmeras novas entram na primeira célula livre. Se o **número de colunas** mudar
(ex.: ao passar de 4 para 5 câmeras o grid vai de 2 para 3 colunas), o layout
salvo é descartado e todos voltam ao mesmo tamanho. Para voltar ao padrão a
qualquer momento, apague o arquivo.

As janelas de gerenciamento (lista, cadastro, edição e varredura) fecham com o
**X** ou com **Esc**; abrir uma de novo cria uma janela nova, já com os dados
atuais.

### Qualidade da imagem

Cada card tem um seletor **Alta / Baixa** na faixa superior (só aparece se o NVR
expõe um substream, ou seja, `substream_index` ≠ `stream` da câmera). *Alta* usa
o stream principal; *Baixa* usa o substream (`app.substream_index`), com menos
CPU e banda. A troca reconecta só aquela câmera. A escolha vale para o grid e é
salva em `~/.config/nvr-dashboard/quality.toml`, sobrepondo `adaptive_stream`
no grid. Em tela cheia, com `adaptive_stream = true` a câmera sempre sobe para
o stream principal e, ao voltar, retorna à qualidade escolhida.

### Indicadores

| Elemento | Significado |
|---|---|
| ● verde | ao vivo |
| ● âmbar | conectando ou aguardando keyframe |
| ● vermelho | reconectando ou falha |
| `REC` | gravando |
| `MOV` | movimento detectado nos últimos 6 s |
| `1920×1080 · 12 fps · 1,8 Mb/s` | resolução, taxa de quadros e bitrate de rede |

---

## Arquitetura

```
src/
├── main.rs         CLI, tracing, runtime do tokio, modo --check
├── config.rs       structs + parsing TOML, Secret (senha nunca vaza em Debug)
├── camera.rs       UrlTemplate, Camera, Redactor de logs
├── pipeline.rs     construção da pipeline GStreamer + StreamStats
├── recording.rs    ramo dinâmico de gravação (tee → parsebin → splitmuxsink)
├── motion.rs       detecção de movimento por diferença de quadros
├── reconnect.rs    Supervisor por câmera: bus, watchdog, backoff, comandos
├── notify.rs       notificações do desktop e ícone na bandeja (ksni)
└── ui/
    ├── mod.rs         janela, ligação pipelines ↔ widgets, canais
    ├── camera_tile.rs widget de uma câmera no grid
    ├── fullscreen.rs  view de câmera única
    ├── grid.rs        layout adaptável
    ├── snapshot.rs    captura PNG via renderer do GTK
    └── style.css      tema escuro
```

### Pipeline por câmera

```
                       ┌─ queue ─▶ decodebin ─▶ tee_raw ─┬─ queue ─▶ gtk4paintablesink
 rtspsrc ─▶ tee_rtp ───┤                                 └─ queue ─▶ videoconvert ─▶ GRAY8 80×45 ─▶ fakesink   (movimento)
                       └─ queue ─▶ parsebin ─▶ splitmuxsink              (gravação, ramo dinâmico)
```

Não há `videoconvert` no caminho de exibição: o sink aceita NV12/DMABuf direto,
então o frame decodificado por hardware não é copiado para a CPU. A conversão
existe só no ramo de movimento.

`tee_rtp` deriva o stream **codificado**: gravar dali evita recodificar e
mantém uma única conexão RTSP com o NVR. O ramo de gravação é adicionado e
removido em runtime; a remoção segue a sequência canônica do GStreamer
(probe `IDLE` → desconecta → injeta `EOS` → remove o bin quando o `EOS` volta
pelo bus), de modo que o arquivo sempre é finalizado.

### Threads e canais

```
 thread GTK                              runtime tokio (2 workers)
 ┌────────────────────┐  CameraEvent   ┌──────────────────────────────┐
 │ tiles · fullscreen │ ◀────────────── │ Supervisor × N câmeras       │
 │ paintables · PNG   │ ───────────────▶│  bus · watchdog · backoff    │
 │ notificações       │    Command      │  health-check · gravação     │
 └─────────┬──────────┘                 └──────────────────────────────┘
           │ Arc<StreamStats> (atômicos: fps, bitrate, resolução, movimento)
           │
     UiAction (widgets → janela)     TraySummary / TrayCommand (bandeja)
```

- A thread do GTK constrói as pipelines (o `GdkPaintable` do sink precisa nascer
  nela), monta o grid e aplica mudanças de estado. Nada de rede ou de I/O toca o
  main loop.
- Um `Supervisor` por câmera roda no tokio, consome o bus do GStreamer como
  stream assíncrono e mantém um watchdog de 1 s.
- Widgets falam com a janela por um canal de `UiAction`, o que evita ciclos `Rc`
  entre o tile e o `Dashboard`.

### Decisões que valem explicar

- **A tela cheia não reparenta widgets.** Um `GdkPaintable` pode ser desenhado
  por vários widgets ao mesmo tempo, então a view de câmera única aponta um
  `gtk::Picture` maior para o mesmo paintable. Sem risco de derrubar a pipeline
  no caminho.
- **A troca de stream só acontece com a pipeline em `NULL`.** O `rtspsrc` ignora
  silenciosamente uma `location` nova enquanto está rodando; o supervisor aplica
  a troca no topo do ciclo, antes de voltar para `PLAYING`.
- **Contadores por sessão.** `session_frames` e a resolução zeram a cada
  tentativa de conexão — sem isso, a sessão anterior faria a nova parecer
  "conectada" antes da hora, com a resolução errada.
- **Gravação sobrevive a quedas.** Se o stream cair no meio de uma gravação, ela
  recomeça sozinha (em arquivo novo) quando o vídeo voltar.
- **Erro na gravação não derruba a visualização.** Um erro vindo de dentro do
  ramo de gravação (disco cheio, por exemplo) desliga só a gravação.
- **O watchdog tem dois relógios.** Um conta bytes vindos do NVR, outro conta
  quadros decodificados. Sem essa separação, o `wait_for_keyframe` faria a
  pipeline parecer travada durante a espera pelo keyframe e o watchdog a
  reiniciaria em loop, sem nunca alcançar o I-frame.
- **Fechar a janela não é imediato.** O `close-request` sinaliza os supervisores
  e segura a janela até todos terminarem (com prazo de 8 s). Dois motivos:
  derrubar a pipeline na hora atropelaria o `EOS` que fecha o arquivo de
  gravação, deixando o vídeo truncado; e o `gtk4paintablesink` guarda objetos
  afins à thread do GTK — pará-lo com o main loop já encerrado faz o glib
  abortar o processo. As pipelines também ficam referenciadas até o
  `process::exit`, que não roda destrutores, para que esse `Drop` nunca caia
  numa thread do tokio.

---

## Segurança

- A senha vive só em `config/cameras.toml` (modo 600, fora do git).
- `Secret` imprime `***` em `Debug`/`Display`; o valor em claro só sai por
  `expose()`, o que torna trivial auditar: `grep -rn 'expose()' src/`.
- `Redactor` limpa as senhas — literal e percent-encoded — de toda mensagem
  vinda do GStreamer antes de ir para o log ou para a UI, porque erros do
  `rtspsrc` ecoam a `location` completa.
- As URLs mostradas em `--check`, nos logs e nos tooltips já vêm mascaradas.

---

## Problemas conhecidos

### Imagem esverdeada nos primeiros segundos

**Causa: o GOP do NVR é longo.** O gravador de referência só emite um keyframe a
cada dezenas de segundos. Quem se conecta no meio desse intervalo não recebe o
quadro de referência, e o decodificador passa a pintar os blocos que chegam
sobre uma superfície zerada — em NV12, zero é verde. A imagem vai "se formando"
devagar até o próximo keyframe e só então fica correta.

Não é bug de driver: os decodificadores por hardware fazem isso porque exibem o
que têm, enquanto os de software costumam segurar a saída. Um H.265 sintético de
mesma resolução decodifica perfeitamente por VA-API.

O dashboard trata o sintoma com `wait_for_keyframe = true` (padrão): o
depayloader segura a saída até um quadro completo e o tile mostra **"Aguardando
keyframe…"** com a explicação, em vez de verde.

**A cura de verdade é no NVR.** No app/web do iCSee/XMEye, em *Encode Config*,
reduza o **I Frame Interval** (GOP) para 1–2× o framerate — com 12 fps, algo
entre 12 e 24. O vídeo passa a aparecer em 1–2 s, e uma perda de pacote deixa de
custar dezenas de segundos de congelamento.

Se preferir ver a imagem se formando aos poucos em vez de esperar, ponha
`wait_for_keyframe = false`.

> **`keyframe_timeout_secs` precisa ser maior que o GOP do seu NVR.** Se for
> menor, o watchdog reinicia a pipeline antes de ela alcançar o keyframe e o
> vídeo nunca aparece.

### Ícone na bandeja no GNOME

O GNOME não implementa `StatusNotifierItem` nativamente. Sem a extensão
[AppIndicator](https://extensions.gnome.org/extension/615/appindicator-support/),
o serviço sobe e o ícone simplesmente não aparece — o resto do dashboard
funciona igual. As notificações do desktop não dependem da extensão.

### Substream

Nem todo firmware expõe `stream=1`. No NVR de referência ele existe (640×360
para canais 1 e 2, 640×720 para o canal 3). Confirme com `--check` e um teste
com `stream = 1` numa câmera antes de ligar `adaptive_stream`.

---

## Diagnóstico

```sh
cargo run --release -- --check                  # config + alcance dos NVRs
RUST_LOG=nvr_dashboard=debug cargo run --release # log detalhado da aplicação
GST_DEBUG=rtspsrc:5 cargo run --release          # log do GStreamer
```

No nível `debug` o app registra as caps negociadas em cada sink, o que resolve a
maior parte dos casos de "conecta mas a imagem está estranha".

Para testar a URL fora do app (a saída de `--check` mostra a URL com a senha
mascarada; substitua o `***`):

```sh
gst-launch-1.0 rtspsrc location="rtsp://…" latency=200 ! decodebin ! autovideosink
```

| Sintoma | Onde olhar |
|---|---|
| `Unauthorized (401)` | usuário/senha em `config/cameras.toml` |
| `SEM RESPOSTA` no `--check` | IP, porta e rede; o NVR está ligado? |
| Imagem esverdeada no início | normal com GOP longo; ver acima. Reduza o I-frame no NVR |
| Fica em "Aguardando keyframe…" para sempre | aumente `keyframe_timeout_secs` acima do GOP do NVR |
| `sem dados do NVR há Ns` | rede instável; tente `rtsp_protocols = "tcp"` e aumente `latency_ms` |
| Gravação não gera arquivo | permissão do diretório; rode com `RUST_LOG=…=debug` e procure "gravação ligada ao muxer" |
| `a câmera ainda não entregou nenhum quadro` ao capturar | normal enquanto o tile mostra "Aguardando keyframe…"; espere o vídeo aparecer |
| Vídeo gravado com duração 0 s | versão antiga; o arquivo só é finalizado se o `EOS` completar — confira se o log traz "gravação finalizada" |

---

## Desenvolvimento

```sh
cargo test                                  # 40 testes unitários
cargo clippy --all-targets -- -D warnings
cargo fmt
make lint                                   # atalho para o clippy acima
```

Os testes cobrem parsing e defaults da configuração, mascaramento de senha,
montagem e mascaramento de URL, resolução de múltiplos NVRs, backoff, layout do
grid, contadores de estatística e a lógica de detecção de movimento. Eles não
precisam de rede nem de um NVR.

O que **não** é coberto por teste automatizado (validado à mão contra o NVR real):
a construção das pipelines GStreamer, o ramo dinâmico de gravação, a troca de
stream e a interface.

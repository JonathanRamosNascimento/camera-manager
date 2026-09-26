#!/usr/bin/env python3
"""Servidor RTSP de mentira, para desenvolver e tirar capturas sem câmera real.

Serve padrões de teste do GStreamer (H.264, 1280×720, 15 fps) em três caminhos:
`/cam1`, `/cam2` e `/cam3`, cada um com um padrão e um nome no canto. Não pede
usuário/senha. Usa só a biblioteca padrão do Python e o `gst-launch-1.0`.

    tools/fake_rtsp.py                 # escuta em 127.0.0.1:8554
    tools/fake_rtsp.py --port 9554
    tools/fake_rtsp.py --image rua.jpg # também serve a foto, parada, em /cam4

O `--image` serve para testar a identificação de objetos: uma foto com pessoas,
carros ou animais dá ao detector algo de verdade para achar (os padrões de teste
não têm nada). Aceita JPEG.

Para o app usar, cadastre um dispositivo `127.0.0.1:8554` com o modelo de URL
`rtsp://{host}:{port}/cam{channel}` (campo "Avançado") e ponha
`rtsp_protocols = "udp"` no `cameras.toml` (este servidor só fala UDP).
"""

import argparse
import socketserver
import subprocess

# caminho → (padrão do videotestsrc, nome exibido no vídeo)
CAMERAS = {
    "cam1": ("pinwheel", "Entrada"),
    "cam2": ("smpte", "Quintal"),
    "cam3": ("colors", "Garagem"),
}

SDP = (
    "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=fake\r\nt=0 0\r\n"
    "m=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=0\r\n"
)


def has_element(name: str) -> bool:
    return subprocess.run(
        ["gst-inspect-1.0", name], capture_output=True
    ).returncode == 0


# Os overlays (pango) faltam em instalações mínimas; sem eles o vídeo sai sem texto.
OVERLAYS = has_element("textoverlay") and has_element("clockoverlay")


# Foto servida em /cam4 (opção --image).
IMAGE: str | None = None


def image_pipeline(path: str, host: str, port: int) -> list[str]:
    return [
        "gst-launch-1.0", "-q",
        "filesrc", f"location={path}", "!", "jpegdec", "!",
        "imagefreeze", "is-live=true", "!",
        "videoconvert", "!", "video/x-raw,framerate=15/1", "!",
        "x264enc", "tune=zerolatency", "speed-preset=ultrafast",
        "key-int-max=15", "bitrate=1500", "!",
        "video/x-h264,profile=baseline", "!",
        "rtph264pay", "config-interval=1", "pt=96", "!",
        "udpsink", f"host={host}", f"port={port}",
    ]


def pipeline(pattern: str, label: str, host: str, port: int) -> list[str]:
    overlays = [
        "textoverlay", f"text={label}", "valignment=bottom", "halignment=left",
        "font-desc=Sans Bold 30", "!",
        "clockoverlay", "halignment=right", "valignment=top",
        "font-desc=Monospace Bold 22", "time-format=%d-%m-%Y %H:%M:%S", "!",
    ] if OVERLAYS else []
    return [
        "gst-launch-1.0", "-q",
        "videotestsrc", "is-live=true", f"pattern={pattern}", "!",
        "video/x-raw,width=1280,height=720,framerate=15/1", "!",
        *overlays,
        "x264enc", "tune=zerolatency", "speed-preset=ultrafast",
        "key-int-max=15", "bitrate=1500", "!",
        "video/x-h264,profile=baseline", "!",
        "rtph264pay", "config-interval=1", "pt=96", "!",
        "udpsink", f"host={host}", f"port={port}",
    ]


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        streams: list[subprocess.Popen] = []
        client = self.client_address[0]
        path, client_port = "cam1", 0
        try:
            while True:
                line = self.rfile.readline().decode(errors="replace").strip()
                if not line:
                    return
                method, url, _ = line.split(" ", 2)
                headers = {}
                while (h := self.rfile.readline().decode(errors="replace").strip()):
                    key, _, value = h.partition(":")
                    headers[key.strip().lower()] = value.strip()
                path = url.rstrip("/").rsplit("/", 1)[-1].split("?")[0]
                if path.startswith("trackID"):  # SETUP/PLAY usam a URL da faixa
                    path = url.split("/")[-2]
                extra, body = [], ""
                if method == "DESCRIBE":
                    extra = ["Content-Type: application/sdp"]
                    body = SDP
                elif method == "SETUP":
                    transport = headers.get("transport", "")
                    if "client_port=" not in transport:
                        self.reply(headers, 461, "Unsupported Transport", [])
                        continue
                    client_port = int(transport.split("client_port=")[1].split("-")[0].split(";")[0])
                    extra = [
                        f"Transport: RTP/AVP;unicast;client_port={client_port}-{client_port + 1};server_port=6970-6971",
                        "Session: 12345678",
                    ]
                elif method == "PLAY":
                    if path == "cam4" and IMAGE:
                        command = image_pipeline(IMAGE, client, client_port)
                    else:
                        pattern, label = CAMERAS.get(path, CAMERAS["cam1"])
                        command = pipeline(pattern, label, client, client_port)
                    streams.append(subprocess.Popen(command))
                    extra = ["Session: 12345678", "Range: npt=0.000-"]
                elif method == "OPTIONS":
                    extra = ["Public: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN"]
                elif method == "TEARDOWN":
                    self.reply(headers, 200, "OK", ["Session: 12345678"])
                    return
                self.reply(headers, 200, "OK", extra, body)
        except (ConnectionError, ValueError):
            pass
        finally:
            for proc in streams:
                proc.terminate()

    def reply(self, headers, code, text, extra, body=""):
        out = [f"RTSP/1.0 {code} {text}", f"CSeq: {headers.get('cseq', '0')}", *extra]
        if body:
            out.append(f"Content-Length: {len(body)}")
        self.wfile.write(("\r\n".join(out) + "\r\n\r\n" + body).encode())


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8554)
    ap.add_argument("--image", help="foto JPEG servida, parada, em /cam4")
    args = ap.parse_args()
    IMAGE = args.image
    with Server((args.host, args.port), Handler) as server:
        print(f"RTSP falso em rtsp://{args.host}:{args.port}/cam1 .. cam3{'  e cam4 (foto)' if IMAGE else ''}  (Ctrl+C sai)", flush=True)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass

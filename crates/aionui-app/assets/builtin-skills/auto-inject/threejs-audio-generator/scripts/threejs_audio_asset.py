#!/usr/bin/env python3
"""Generate and process Three.js game audio assets.

Music/SFX/isolate/voice-change: ElevenLabs.
Spoken TTS: Volcengine seed-tts-2.0 (Plan unidirectional HTTP).
Prefer the Node twin `threejs_audio_asset.mjs` for probe, kit, and music.
This file remains for sfx/tts/isolate/voice-change fallback. Never run bare `python`.
"""

from __future__ import annotations

import argparse
import base64
import json
import mimetypes
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path
from typing import Any


BASE_URL = "https://api.elevenlabs.io/v1"
DEFAULT_OUTPUT_FORMAT = "mp3_44100_128"
DEFAULT_TTS_VOICE_ID = "JBFqnCBsd6RMkjVDRZzb"
SEED_TTS_URL = "https://openspeech.bytedance.com/api/v3/plan/tts/unidirectional"
SEED_TTS_RESOURCE_ID = "seed-tts-2.0"
DEFAULT_SEED_TTS_SPEAKER = "zh_female_vv_uranus_bigtts"


class AudioGeneratorError(RuntimeError):
    pass


def eprint(message: str) -> None:
    print(message, file=sys.stderr)


def api_key(args: argparse.Namespace) -> str:
    key = getattr(args, "api_key", None) or os.environ.get("ELEVENLABS_API_KEY")
    if not key:
        raise AudioGeneratorError("Missing API key. Set ELEVENLABS_API_KEY or pass --api-key.")
    return key


def seed_tts_key(args: argparse.Namespace) -> str:
    key = (
        getattr(args, "api_key", None)
        or os.environ.get("SEED_TTS_API_KEY")
        or os.environ.get("AIONUI_BUILTIN_ARK_IMAGE_PLAN_API_KEY")
    )
    if not key:
        raise AudioGeneratorError("Missing TTS API key. Set SEED_TTS_API_KEY or pass --api-key.")
    return str(key).strip()


def parse_concatenated_json(text: str) -> list[Any]:
    objects: list[Any] = []
    i = 0
    n = len(text)
    while i < n:
        while i < n and text[i].isspace():
            i += 1
        if i >= n:
            break
        start = i
        depth = 0
        in_string = False
        escaped = False
        while i < n:
            c = text[i]
            if in_string:
                if escaped:
                    escaped = False
                elif c == "\\":
                    escaped = True
                elif c == '"':
                    in_string = False
            elif c == '"':
                in_string = True
            elif c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    objects.append(json.loads(text[start : i + 1]))
                    i += 1
                    break
            i += 1
        else:
            break
    return objects


def mp3_from_tts_stream(text: str) -> bytes:
    parts: list[bytes] = []
    for obj in parse_concatenated_json(text):
        code = obj.get("code")
        if code not in (None, 0, 20000000):
            raise AudioGeneratorError(f"TTS {code}: {obj.get('message') or 'error'}")
        data = obj.get("data")
        if isinstance(data, str) and data:
            parts.append(base64.b64decode(data))
    if not parts:
        raise AudioGeneratorError("TTS returned no audio")
    return b"".join(parts)


def build_url(path: str, query: dict[str, Any] | None = None) -> str:
    url = f"{BASE_URL}{path}"
    clean = {key: value for key, value in (query or {}).items() if value is not None}
    if clean:
        url = f"{url}?{urllib.parse.urlencode(clean)}"
    return url


def request_bytes(
    method: str,
    path: str,
    key: str,
    body: bytes | None = None,
    headers: dict[str, str] | None = None,
    query: dict[str, Any] | None = None,
    timeout: int = 300,
) -> bytes:
    req = urllib.request.Request(build_url(path, query), data=body, method=method)
    req.add_header("xi-api-key", key)
    for name, value in (headers or {}).items():
        req.add_header(name, value)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.read()
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")
        raise AudioGeneratorError(f"HTTP {exc.code}: {detail}") from exc
    except urllib.error.URLError as exc:
        raise AudioGeneratorError(f"Network error: {exc.reason}") from exc


def post_json_audio(args: argparse.Namespace, path: str, payload: dict[str, Any], out: Path) -> None:
    body = json.dumps(payload).encode("utf-8")
    data = request_bytes(
        "POST",
        path,
        api_key(args),
        body=body,
        headers={"Content-Type": "application/json", "Accept": "audio/mpeg"},
        query={"output_format": args.output_format},
    )
    write_file(out, data)


def multipart_body(fields: dict[str, Any], files: dict[str, Path]) -> tuple[bytes, str]:
    boundary = f"----threejs-audio-{uuid.uuid4().hex}"
    chunks: list[bytes] = []

    for name, value in fields.items():
        if value is None:
            continue
        if isinstance(value, bool):
            value = "true" if value else "false"
        chunks.append(f"--{boundary}\r\n".encode())
        chunks.append(f'Content-Disposition: form-data; name="{name}"\r\n\r\n'.encode())
        chunks.append(str(value).encode())
        chunks.append(b"\r\n")

    for name, path in files.items():
        if not path.exists():
            raise AudioGeneratorError(f"Input file not found: {path}")
        mime = mimetypes.guess_type(path.name)[0] or "application/octet-stream"
        chunks.append(f"--{boundary}\r\n".encode())
        chunks.append(
            f'Content-Disposition: form-data; name="{name}"; filename="{path.name}"\r\n'.encode()
        )
        chunks.append(f"Content-Type: {mime}\r\n\r\n".encode())
        chunks.append(path.read_bytes())
        chunks.append(b"\r\n")

    chunks.append(f"--{boundary}--\r\n".encode())
    return b"".join(chunks), boundary


def post_multipart_audio(
    args: argparse.Namespace,
    path: str,
    fields: dict[str, Any],
    files: dict[str, Path],
    out: Path,
    query: dict[str, Any] | None = None,
) -> None:
    body, boundary = multipart_body(fields, files)
    data = request_bytes(
        "POST",
        path,
        api_key(args),
        body=body,
        headers={"Content-Type": f"multipart/form-data; boundary={boundary}", "Accept": "audio/mpeg"},
        query=query,
    )
    write_file(out, data)


def write_file(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    print(f"Audio saved: {path.resolve()}")


def voice_settings(args: argparse.Namespace) -> dict[str, Any] | None:
    settings: dict[str, Any] = {}
    for field in ("stability", "similarity_boost", "style"):
        value = getattr(args, field, None)
        if value is not None:
            settings[field] = value
    if getattr(args, "speaker_boost", False):
        settings["use_speaker_boost"] = True
    return settings or None


def cmd_probe(args: argparse.Namespace) -> int:
    eleven = "SET" if (args.api_key or os.environ.get("ELEVENLABS_API_KEY")) else "MISSING"
    tts = (
        "SET"
        if (
            os.environ.get("SEED_TTS_API_KEY")
            or os.environ.get("AIONUI_BUILTIN_ARK_IMAGE_PLAN_API_KEY")
        )
        else "MISSING"
    )
    print(f"ELEVENLABS_API_KEY={eleven}")
    print(f"SEED_TTS_API_KEY={tts}")
    if args.validate and eleven == "SET":
        data = request_bytes("GET", "/user", api_key(args))
        user = json.loads(data.decode("utf-8"))
        print(f"VALID_USER={user.get('email') or user.get('user_id') or 'ok'}")
    return 0


def cmd_sfx(args: argparse.Namespace) -> int:
    payload: dict[str, Any] = {
        "text": args.prompt,
        "model_id": args.model_id,
        "prompt_influence": args.prompt_influence,
        "loop": args.loop,
    }
    if args.duration is not None:
        payload["duration_seconds"] = args.duration
    post_json_audio(args, "/sound-generation", payload, Path(args.out))
    return 0


def resolve_seed_tts_speaker(speaker: str | None) -> str:
    ident = (speaker or "").strip()
    lower = ident.lower()
    if "uranus_bigtts" in lower or "saturn_bigtts" in lower or lower.startswith("saturn_"):
        return ident
    return DEFAULT_SEED_TTS_SPEAKER


def cmd_tts(args: argparse.Namespace) -> int:
    payload: dict[str, Any] = {
        "user": {"uid": "aion-game-audio"},
        "req_params": {
            "text": args.text,
            "speaker": resolve_seed_tts_speaker(args.voice_id),
            "audio_params": {"format": "mp3", "sample_rate": 24000, "bit_rate": 128000},
        },
    }
    req = urllib.request.Request(SEED_TTS_URL, data=json.dumps(payload).encode("utf-8"), method="POST")
    req.add_header("Content-Type", "application/json")
    req.add_header("X-Api-Key", seed_tts_key(args))
    req.add_header("X-Api-Resource-Id", SEED_TTS_RESOURCE_ID)
    req.add_header("X-Api-Request-Id", str(uuid.uuid4()))
    try:
        with urllib.request.urlopen(req, timeout=180) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")
        raise AudioGeneratorError(f"HTTP {exc.code}: {detail}") from exc
    except urllib.error.URLError as exc:
        raise AudioGeneratorError(f"Network error: {exc.reason}") from exc
    write_file(Path(args.out), mp3_from_tts_stream(raw))
    return 0


def cmd_isolate(args: argparse.Namespace) -> int:
    fields = {"file_format": args.file_format}
    post_multipart_audio(
        args,
        "/audio-isolation",
        fields,
        {"audio": Path(args.input)},
        Path(args.out),
        query={"output_format": args.output_format},
    )
    return 0


def cmd_voice_change(args: argparse.Namespace) -> int:
    fields: dict[str, Any] = {
        "model_id": args.model_id,
        "remove_background_noise": args.remove_background_noise,
        "file_format": args.file_format,
    }
    settings = voice_settings(args)
    if settings:
        fields["voice_settings"] = json.dumps(settings)
    if args.seed is not None:
        fields["seed"] = args.seed
    post_multipart_audio(
        args,
        f"/speech-to-speech/{urllib.parse.quote(args.voice_id)}",
        fields,
        {"audio": Path(args.input)},
        Path(args.out),
        query={
            "output_format": args.output_format,
            "optimize_streaming_latency": args.optimize_streaming_latency,
        },
    )
    return 0


def add_common(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--api-key", help="ElevenLabs API key; defaults to ELEVENLABS_API_KEY")


def add_output_format(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--output-format", default=DEFAULT_OUTPUT_FORMAT)


def add_voice_settings(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--stability", type=float)
    parser.add_argument("--similarity-boost", type=float)
    parser.add_argument("--style", type=float)
    parser.add_argument("--speaker-boost", action="store_true")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Generate and process Three.js game audio assets.")
    sub = parser.add_subparsers(dest="command", required=True)

    probe = sub.add_parser("probe", help="Report whether ELEVENLABS_API_KEY is available.")
    add_common(probe)
    probe.add_argument("--validate", action="store_true", help="Call /user to validate the key.")
    probe.set_defaults(func=cmd_probe)

    sfx = sub.add_parser("sfx", help="Generate sound effects or ambience from a prompt.")
    add_common(sfx)
    add_output_format(sfx)
    sfx.add_argument("--prompt", required=True)
    sfx.add_argument("--out", required=True)
    sfx.add_argument("--duration", type=float, help="Duration in seconds, typically 0.5-30.")
    sfx.add_argument("--prompt-influence", type=float, default=0.55)
    sfx.add_argument("--loop", action="store_true")
    sfx.add_argument("--model-id", default="eleven_text_to_sound_v2")
    sfx.set_defaults(func=cmd_sfx)

    tts = sub.add_parser("tts", help="Generate a spoken line from text.")
    add_common(tts)
    add_output_format(tts)
    add_voice_settings(tts)
    tts.add_argument("--text", required=True)
    tts.add_argument("--out", required=True)
    tts.add_argument("--voice-id", default=DEFAULT_SEED_TTS_SPEAKER)
    tts.add_argument("--model-id", default="eleven_multilingual_v2")
    tts.set_defaults(func=cmd_tts)

    isolate = sub.add_parser("isolate", help="Clean or isolate speech from a source audio file.")
    add_common(isolate)
    add_output_format(isolate)
    isolate.add_argument("--input", required=True)
    isolate.add_argument("--out", required=True)
    isolate.add_argument("--file-format", default="other")
    isolate.set_defaults(func=cmd_isolate)

    voice = sub.add_parser("voice-change", help="Convert source performance to a target voice.")
    add_common(voice)
    add_output_format(voice)
    add_voice_settings(voice)
    voice.add_argument("--input", required=True)
    voice.add_argument("--out", required=True)
    voice.add_argument("--voice-id", default=DEFAULT_TTS_VOICE_ID)
    voice.add_argument("--model-id", default="eleven_multilingual_sts_v2")
    voice.add_argument("--file-format", default="other")
    voice.add_argument("--seed", type=int)
    voice.add_argument("--remove-background-noise", action="store_true")
    voice.add_argument("--optimize-streaming-latency", type=int, choices=range(0, 5))
    voice.set_defaults(func=cmd_voice_change)

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    try:
        return args.func(args)
    except AudioGeneratorError as exc:
        eprint(f"threejs_audio_asset.py: {exc}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

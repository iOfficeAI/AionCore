"""Mureka AI 音乐生成命令行工具，统一提供歌曲、纯音乐、歌词和上传操作。"""

import argparse
import json
import os
import sys
import time

import requests


class ChineseHelpFormatter(argparse.RawDescriptionHelpFormatter):
    """将 argparse 的固定帮助标题本地化为简体中文。"""


class ChineseArgumentParser(argparse.ArgumentParser):
    """使用简体中文帮助标题和常见参数错误的解析器。"""

    def __init__(self, *args, **kwargs):
        kwargs.setdefault("formatter_class", ChineseHelpFormatter)
        super().__init__(*args, **kwargs)

    def format_help(self):
        self._positionals.title = "位置参数"
        self._optionals.title = "选项"
        for action in self._actions:
            if isinstance(action, argparse._HelpAction):
                action.help = "显示此帮助信息并退出"
        help_text = super().format_help()
        return help_text.replace("usage: ", "用法：", 1)

    def print_usage(self, file=None):
        usage_text = super().format_usage().replace("usage: ", "用法：", 1)
        self._print_message(usage_text, file)

    def error(self, message):
        replacements = {
            "the following arguments are required:": "缺少以下必需参数：",
            "unrecognized arguments:": "无法识别的参数：",
            "invalid choice:": "无效选项：",
            "choose from": "可选值为",
            "expected one argument": "需要一个参数",
            "argument ": "参数 ",
        }
        for source, target in replacements.items():
            message = message.replace(source, target)
        self.print_usage(sys.stderr)
        self.exit(2, f"{self.prog}: 错误：{message}\n")


# ---------------------------------------------------------------------------
# API 客户端
# ---------------------------------------------------------------------------

API_BASE = os.getenv("MUREKA_API_URL", "https://api.mureka.ai").rstrip("/")
POLL_INTERVAL = 5
POLL_TIMEOUT = 600


def get_api_key():
    key = os.getenv("MUREKA_API_KEY")
    if not key:
        print("错误：未设置 MUREKA_API_KEY", file=sys.stderr)
        sys.exit(1)
    return key


def headers(api_key=None):
    key = api_key or get_api_key()
    return {"Authorization": f"Bearer {key}", "Content-Type": "application/json"}


def post_json(path, payload, api_key=None, timeout=60):
    url = f"{API_BASE}{path}"
    resp = requests.post(url, json=payload, headers=headers(api_key), timeout=timeout)
    if not resp.ok:
        raise requests.HTTPError(f"{resp.status_code} {resp.reason}: {resp.text}", response=resp)
    return resp.json()


def get_json(path, api_key=None, timeout=30):
    url = f"{API_BASE}{path}"
    resp = requests.get(url, headers=headers(api_key), timeout=timeout)
    if not resp.ok:
        raise requests.HTTPError(f"{resp.status_code} {resp.reason}: {resp.text}", response=resp)
    return resp.json()


def upload_file_api(file_path, purpose, api_key=None):
    key = api_key or get_api_key()
    url = f"{API_BASE}/v1/files/upload"
    with open(file_path, "rb") as f:
        resp = requests.post(
            url,
            headers={"Authorization": f"Bearer {key}"},
            files={"file": f},
            data={"purpose": purpose},
            timeout=120,
        )
    resp.raise_for_status()
    return resp.json()


def poll_task(query_path, task_id, api_key=None, interval=POLL_INTERVAL, timeout=POLL_TIMEOUT):
    key = api_key or get_api_key()
    path = f"{query_path}/{task_id}"
    deadline = time.time() + timeout
    terminal = {"succeeded", "failed", "timeouted", "cancelled"}

    while True:
        data = get_json(path, api_key=key)
        status = data.get("status", "")
        print(f"  [{status}] 任务 {task_id}", file=sys.stderr)

        if status in terminal:
            if status != "succeeded":
                reason = data.get("failed_reason", status)
                raise RuntimeError(f"任务 {task_id} 结束，状态：{status}。原因：{reason}")
            return data

        if time.time() > deadline:
            raise RuntimeError(f"任务 {task_id} 在 {timeout} 秒后超时（最后状态：{status}）")

        time.sleep(interval)


def download_audio(url, output_path):
    resp = requests.get(url, timeout=120)
    resp.raise_for_status()
    os.makedirs(os.path.dirname(os.path.abspath(output_path)), exist_ok=True)
    with open(output_path, "wb") as f:
        f.write(resp.content)
    return output_path


def download_choices(result, args):
    """将音频文件下载到输出目录。"""
    choices = result.get("choices", [])
    if not choices:
        print("未生成任何结果。", file=sys.stderr)
        sys.exit(1)

    out_dir = args.output
    os.makedirs(out_dir, exist_ok=True)

    format_key = {"mp3": "url", "flac": "flac_url", "wav": "wav_url"}[args.format]

    for choice in choices:
        idx = choice.get("index", 0)
        url = choice.get(format_key) or choice.get("url")
        duration_ms = choice.get("duration", 0)

        if len(choices) > 1:
            filename = f"audio_{idx}.{args.format}"
        else:
            filename = f"audio.{args.format}"

        output_path = os.path.join(out_dir, filename)
        print(f"正在下载候选 {idx}（{duration_ms / 1000:.1f} 秒）→ {output_path}", file=sys.stderr)
        download_audio(url, output_path)
        print(output_path)


# ---------------------------------------------------------------------------
# 子命令
# ---------------------------------------------------------------------------

def cmd_song(args):
    """生成带歌词和人声的歌曲。"""
    payload = {"lyrics": args.lyrics, "model": args.model}
    if args.prompt:
        payload["prompt"] = args.prompt
    if args.reference_id:
        payload["reference_id"] = args.reference_id
    if args.vocal_id:
        payload["vocal_id"] = args.vocal_id
    if args.melody_id:
        payload["melody_id"] = args.melody_id
    if args.n:
        payload["n"] = args.n

    # 将歌词和提示词保存到输出目录
    os.makedirs(args.output, exist_ok=True)
    lyrics_path = os.path.join(args.output, "lyrics.txt")
    with open(lyrics_path, "w", encoding="utf-8") as f:
        f.write(args.lyrics)
        if args.prompt:
            f.write(f"\n\n---\n提示词：{args.prompt}\n")
    print(f"歌词已保存 → {lyrics_path}", file=sys.stderr)

    print("正在提交歌曲生成任务……", file=sys.stderr)
    task = post_json("/v1/song/generate", payload)
    task_id = task["id"]
    print(f"任务 ID：{task_id}", file=sys.stderr)

    print("正在轮询任务状态……", file=sys.stderr)
    result = poll_task("/v1/song/query", task_id,
                       interval=args.poll_interval, timeout=args.poll_timeout)
    download_choices(result, args)


def cmd_instrumental(args):
    """生成纯音乐。"""
    payload = {"model": args.model}
    if args.prompt:
        payload["prompt"] = args.prompt
    if args.instrumental_id:
        payload["instrumental_id"] = args.instrumental_id
    if args.n:
        payload["n"] = args.n

    print("正在提交纯音乐生成任务……", file=sys.stderr)
    task = post_json("/v1/instrumental/generate", payload)
    task_id = task["id"]
    print(f"任务 ID：{task_id}", file=sys.stderr)

    print("正在轮询任务状态……", file=sys.stderr)
    result = poll_task("/v1/instrumental/query", task_id,
                       interval=args.poll_interval, timeout=args.poll_timeout)
    download_choices(result, args)


def cmd_lyrics(args):
    """生成或扩写歌词。"""
    if args.lyrics_command == "generate":
        result = post_json("/v1/lyrics/generate", {"prompt": args.prompt})
        title = result.get("title", "")
        lyrics = result.get("lyrics", "")
        if title:
            print(f"标题：{title}\n")
        print(lyrics)
    elif args.lyrics_command == "extend":
        result = post_json("/v1/lyrics/extend", {"lyrics": args.lyrics})
        print(result.get("lyrics", ""))


def cmd_upload(args):
    """向 Mureka 上传文件。"""
    print(f"正在上传 {args.file}（purpose={args.purpose}）……", file=sys.stderr)
    result = upload_file_api(args.file, args.purpose)
    file_id = result.get("id", "")
    print(f"文件 ID：{file_id}")
    print(json.dumps(result, indent=2))


# ---------------------------------------------------------------------------
# 通用参数辅助函数
# ---------------------------------------------------------------------------

def add_generation_args(parser, default_output="./output"):
    """添加通用生成参数。"""
    parser.add_argument("--model", default="mureka-8",
                        help="模型（默认：mureka-8）")
    parser.add_argument("-n", "--n", type=int, default=None, dest="n",
                        help="生成结果数量（默认 2，最多 3）")
    parser.add_argument("--output", default=default_output,
                        help=f"输出目录（默认：{default_output}）")
    parser.add_argument("--format", choices=["mp3", "flac", "wav"], default="mp3",
                        help="下载格式（默认：mp3）")
    parser.add_argument("--poll-interval", type=int, default=5,
                        help="轮询间隔秒数（默认：5）")
    parser.add_argument("--poll-timeout", type=int, default=600,
                        help="轮询超时秒数（默认：600）")


# ---------------------------------------------------------------------------
# 主入口
# ---------------------------------------------------------------------------

def main():
    parser = ChineseArgumentParser(
        description="Mureka AI 音乐生成命令行工具",
        epilog="""示例：
  %(prog)s song --lyrics "[Verse]\\nHello world" --prompt "pop, 120 BPM, female vocal"
  %(prog)s instrumental --prompt "ambient, 80 BPM, soft pads"
  %(prog)s lyrics generate "a summer love song"
  %(prog)s lyrics extend "[Verse]\\nExisting lyrics..."
  %(prog)s upload reference.mp3 --purpose reference
""")
    sub = parser.add_subparsers(dest="command", required=True)

    # --- 歌曲 ---
    p_song = sub.add_parser("song", help="生成带歌词和人声的歌曲")
    p_song.add_argument("--lyrics", required=True,
                        help="歌曲歌词（最多 3000 字符），使用 [Verse]、[Chorus] 等结构标签")
    p_song.add_argument("--prompt", default=None,
                        help="风格或场景 prompt（最多 1024 字符）")
    p_song.add_argument("--reference-id", default=None,
                        help="参考音频文件 ID（purpose=reference）")
    p_song.add_argument("--vocal-id", default=None,
                        help="已有 Vocal ID（不是文件上传 purpose）")
    p_song.add_argument("--melody-id", default=None,
                        help="旋律文件 ID（purpose=melody），不能与其他控制选项同时使用")
    add_generation_args(p_song, "./output")

    # --- 纯音乐 ---
    p_inst = sub.add_parser("instrumental", help="生成纯音乐")
    p_inst.add_argument("--prompt", default=None,
                        help="风格或场景 prompt（最多 1024 字符）")
    p_inst.add_argument("--instrumental-id", default=None,
                        help="参考纯音乐文件 ID（purpose=instrumental）")
    add_generation_args(p_inst, "./instrumental")

    # --- 歌词 ---
    p_lyrics = sub.add_parser("lyrics", help="生成或扩写歌词")
    lyrics_sub = p_lyrics.add_subparsers(dest="lyrics_command", required=True)

    p_lyrics_gen = lyrics_sub.add_parser("generate", help="根据 prompt 生成歌词")
    p_lyrics_gen.add_argument("prompt", help="歌词主题或描述")

    p_lyrics_ext = lyrics_sub.add_parser("extend", help="扩写现有歌词")
    p_lyrics_ext.add_argument("lyrics", help="要继续扩写的现有歌词")

    # --- 上传 ---
    p_upload = sub.add_parser("upload", help="向 Mureka 上传文件")
    p_upload.add_argument("file", help="上传文件路径；格式和时长须符合所选 purpose")
    p_upload.add_argument("--purpose", required=True,
                          choices=["reference", "melody", "instrumental", "voice",
                                   "audio", "remix", "soundtrack", "lyrics-video"],
                          help="用途：reference、melody、instrumental、voice、audio、remix、soundtrack 或 lyrics-video；具体格式和限制见 skill 文档")

    args = parser.parse_args()

    {"song": cmd_song,
     "instrumental": cmd_instrumental,
     "lyrics": cmd_lyrics,
     "upload": cmd_upload}[args.command](args)


if __name__ == "__main__":
    main()

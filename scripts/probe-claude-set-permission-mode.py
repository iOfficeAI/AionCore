#!/usr/bin/env python3
"""Capture real claude-CLI wire traffic for `control_request{set_permission_mode}`.

Settles three questions that aionCore currently answers from an uncited rationale:

  Q1  Does a mid-turn set_permission_mode truncate / reinitialize the in-flight turn?
      claude_conn.rs:432-440 asserts it does ("raw-CLI limitation") but cites no
      fixture, while every other live-probed claim in that file cites a sample path.
  Q2  Does a USER-DRIVEN set emit `{"type":"system",...,"permissionMode":...}`?
      `sniff_mode` (claude_conn.rs:2024) is the ONLY mode-confirmation path and reads
      exactly that frame. If a user-driven set does not emit one, mode switches are
      never confirmed at the backend layer.
  Q3  What does the success control_response literally contain? claude_conn.rs:1517
      claims the ack echoes `response.mode`, yet no code reads it
      (`sniff_set_mode_response` does not exist -- the name appears only in comments).

Argv mirrors ClaudeAdapter::spawn (adapter/claude.rs:1015-1059) plus the
`--permission-mode` appended by build_claude_init_args (claude_conn.rs:169-208).
The can_use_tool auto-reply mirrors build_control_response (claude_conn.rs:1245-1285):
`updatedInput` MUST be a record and `toolUseID` MUST be echoed, or claude's ZodError
rejects the frame and the approved tool silently never runs -- which would look like
a truncated turn and be misread as evidence for Q1.

Usage:
    python3 scripts/probe-claude-set-permission-mode.py --scenario all --outdir /tmp/cap
"""

from __future__ import annotations

import argparse
import asyncio
import json
import shutil
import sys
import time
import uuid
from pathlib import Path

SPAWN_ARGS = [
    "--print",
    "--input-format", "stream-json",
    "--output-format", "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--replay-user-messages",
    "--permission-prompt-tool", "stdio",
]

# A prompt that streams for a while AND ends in a tool call, so one turn exercises
# both the truncation question and the "did the new mode apply to this turn" question.
TOOL_PROMPT = (
    "Count from 1 to 30, one number per line. "
    "Then create a file named probe.txt containing exactly the word hello."
)


class Probe:
    def __init__(self, claude_bin: str, workdir: Path, outdir: Path, scenario: str):
        self.claude_bin = claude_bin
        self.workdir = workdir
        self.outdir = outdir
        self.scenario = scenario
        self.proc: asyncio.subprocess.Process | None = None
        self.trace = (outdir / f"{scenario}.trace.jsonl").open("w")
        self.inbound = (outdir / f"{scenario}.inbound.jsonl").open("w")
        self.ctl_seq = 0
        self.t0 = time.monotonic()
        # Observations feeding the verdict.
        self.saw_first_delta = asyncio.Event()
        self.deep_in_stream = asyncio.Event()   # set once generation is visibly flowing
        self.delta_count = 0
        # claude streams in coarse chunks (~15 content deltas for a 30-line answer),
        # so this is deliberately low: it must land mid-generation, not never.
        self.deep_delta_threshold = 5
        self.mode_ctl_ids: dict[str, str] = {}   # request_id -> requested mode
        self.control_responses: list[dict] = []
        self.system_frames: list[dict] = []
        self.can_use_tool_all: list[dict] = []
        self.can_use_tool_after_switch: list[dict] = []
        self.switch_sent_at: float | None = None
        self.result_frame: dict | None = None
        self.text_after_switch = 0
        self.errors: list[str] = []

    def _stamp(self) -> float:
        return round(time.monotonic() - self.t0, 3)

    def _log(self, direction: str, frame: dict) -> None:
        rec = {"t": self._stamp(), "dir": direction, "frame": frame}
        self.trace.write(json.dumps(rec) + "\n")
        self.trace.flush()
        if direction == "in":
            self.inbound.write(json.dumps(frame) + "\n")
            self.inbound.flush()

    async def spawn(self, permission_mode: str) -> None:
        args = [
            *SPAWN_ARGS,
            "--session-id", str(uuid.uuid4()),
            "--permission-mode", permission_mode,
        ]
        print(f"[spawn] mode={permission_mode}", flush=True)
        self.proc = await asyncio.create_subprocess_exec(
            self.claude_bin, *args,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            cwd=str(self.workdir),
        )

    async def send(self, frame: dict) -> None:
        assert self.proc and self.proc.stdin
        self._log("out", frame)
        self.proc.stdin.write((json.dumps(frame) + "\n").encode())
        await self.proc.stdin.drain()

    async def send_prompt(self, text: str) -> None:
        await self.send({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        })

    async def send_set_mode(self, mode: str) -> str:
        self.ctl_seq += 1
        request_id = f"ctl-{self.ctl_seq}"
        self.mode_ctl_ids[request_id] = mode
        self.switch_sent_at = self._stamp()
        await self.send({
            "type": "control_request",
            "request_id": request_id,
            "request": {"subtype": "set_permission_mode", "mode": mode},
        })
        print(f"[switch] t={self.switch_sent_at}s -> {mode} ({request_id})", flush=True)
        return request_id

    async def _answer_can_use_tool(self, frame: dict) -> None:
        """Mirror build_control_response (claude_conn.rs:1245-1285) exactly."""
        request_id = frame.get("request_id")
        req = frame.get("request") or {}
        tool_use_id = req.get("tool_use_id")
        if not request_id or not tool_use_id:
            self.errors.append(f"can_use_tool missing ids: {json.dumps(frame)[:300]}")
            return
        raw_input = req.get("input")
        updated_input = raw_input if isinstance(raw_input, dict) else {}
        await self.send({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": {
                    "behavior": "allow",
                    "updatedInput": updated_input,
                    "toolUseID": tool_use_id,
                },
            },
        })

    def _classify(self, frame: dict) -> None:
        ftype = frame.get("type")
        after_switch = self.switch_sent_at is not None

        if ftype == "system":
            self.system_frames.append(frame)
            if "permissionMode" in frame:
                print(f"[system] t={self._stamp()}s subtype={frame.get('subtype')} "
                      f"permissionMode={frame.get('permissionMode')}", flush=True)
        elif ftype == "result":
            self.result_frame = frame
        elif ftype == "control_response":
            resp = frame.get("response") or {}
            if resp.get("request_id") in self.mode_ctl_ids:
                self.control_responses.append(frame)
                print(f"[ack] t={self._stamp()}s {json.dumps(frame)}", flush=True)
        elif ftype == "control_request":
            if (frame.get("request") or {}).get("subtype") == "can_use_tool":
                self.can_use_tool_all.append(frame)
                if after_switch:
                    self.can_use_tool_after_switch.append(frame)

        # Any assistant text proves the turn is alive.
        if ftype in ("stream_event", "assistant"):
            if not self.saw_first_delta.is_set():
                self.saw_first_delta.set()
            if after_switch:
                self.text_after_switch += 1
        # `message_start` fires before any content exists; only a content_block_delta
        # proves the model is actively emitting, which is what "mid-generation" means.
        if ftype == "stream_event" and (frame.get("event") or {}).get("type") == "content_block_delta":
            self.delta_count += 1
            if self.delta_count >= self.deep_delta_threshold and not self.deep_in_stream.is_set():
                self.deep_in_stream.set()

    async def read_stdout(self) -> None:
        assert self.proc and self.proc.stdout
        async for raw in self.proc.stdout:
            line = raw.decode("utf-8", "replace").strip()
            if not line:
                continue
            try:
                frame = json.loads(line)
            except json.JSONDecodeError:
                self.trace.write(json.dumps({"t": self._stamp(), "dir": "in-raw",
                                             "line": line[:2000]}) + "\n")
                continue
            self._log("in", frame)
            self._classify(frame)
            if frame.get("type") == "control_request" and \
               (frame.get("request") or {}).get("subtype") == "can_use_tool":
                await self._answer_can_use_tool(frame)

    async def read_stderr(self) -> None:
        assert self.proc and self.proc.stderr
        async for raw in self.proc.stderr:
            msg = raw.decode("utf-8", "replace").rstrip()
            if msg:
                self.errors.append(msg)
                print(f"[stderr] {msg}", file=sys.stderr, flush=True)

    def verdict(self) -> dict:
        mode_systems = [f for f in self.system_frames if "permissionMode" in f]
        return {
            "scenario": self.scenario,
            "Q1_turn_survived_switch": {
                "reached_result_frame": self.result_frame is not None,
                "assistant_frames_after_switch": self.text_after_switch,
                "result_subtype": (self.result_frame or {}).get("subtype"),
                "is_error": (self.result_frame or {}).get("is_error"),
            },
            "Q2_system_frames_with_permissionMode": mode_systems,
            "Q2_count": len(mode_systems),
            "Q3_control_responses_for_our_set": self.control_responses,
            "can_use_tool_total": len(self.can_use_tool_all),
            "can_use_tool_after_switch": len(self.can_use_tool_after_switch),
            "tools_prompted_after_switch": [
                (f.get("request") or {}).get("tool_name")
                for f in self.can_use_tool_after_switch
            ],
            "stderr": self.errors[:20],
        }

    async def finish(self) -> dict:
        if self.proc and self.proc.stdin and not self.proc.stdin.is_closing():
            try:
                self.proc.stdin.close()
            except Exception:
                pass
        if self.proc:
            try:
                await asyncio.wait_for(self.proc.wait(), timeout=10)
            except asyncio.TimeoutError:
                self.proc.kill()
        v = self.verdict()
        (self.outdir / f"{self.scenario}.verdict.json").write_text(json.dumps(v, indent=2))
        self.trace.close()
        self.inbound.close()
        return v


async def scenario_idle(p: Probe) -> None:
    """S1 baseline: switch while NO turn is running. Isolates Q2/Q3 from turn state."""
    await p.spawn("default")
    reader = asyncio.create_task(p.read_stdout())
    errs = asyncio.create_task(p.read_stderr())
    await asyncio.sleep(3)          # let system/init land
    await p.send_set_mode("plan")
    await asyncio.sleep(8)          # watch for an ack and/or a system/status
    reader.cancel()
    errs.cancel()


async def scenario_midturn(p: Probe) -> None:
    """S2, the key one: switch WHILE a turn streams, then watch the turn's fate."""
    await p.spawn("default")
    reader = asyncio.create_task(p.read_stdout())
    errs = asyncio.create_task(p.read_stderr())
    await asyncio.sleep(2)
    await p.send_prompt(TOOL_PROMPT)
    try:
        await asyncio.wait_for(p.saw_first_delta.wait(), timeout=90)
    except asyncio.TimeoutError:
        p.errors.append("no assistant delta within 90s -- turn never started")
        reader.cancel()
        errs.cancel()
        return
    print(f"[turn] first delta at t={p._stamp()}s; switching mid-stream", flush=True)
    await p.send_set_mode("acceptEdits")
    # Ride the turn to its natural end: a truncation shows up as a missing/errored
    # result frame or a sudden stop in assistant frames.
    for _ in range(180):
        if p.result_frame is not None:
            break
        await asyncio.sleep(1)
    await asyncio.sleep(3)
    reader.cancel()
    errs.cancel()


async def scenario_plan_exit(p: Probe) -> None:
    """S3 symmetry: start in plan, switch to default mid-turn."""
    await p.spawn("plan")
    reader = asyncio.create_task(p.read_stdout())
    errs = asyncio.create_task(p.read_stderr())
    await asyncio.sleep(2)
    await p.send_prompt("Count from 1 to 20, one number per line.")
    try:
        await asyncio.wait_for(p.saw_first_delta.wait(), timeout=90)
    except asyncio.TimeoutError:
        p.errors.append("no assistant delta within 90s")
        reader.cancel()
        errs.cancel()
        return
    await p.send_set_mode("default")
    for _ in range(120):
        if p.result_frame is not None:
            break
        await asyncio.sleep(1)
    await asyncio.sleep(3)
    reader.cancel()
    errs.cancel()


async def scenario_control_no_switch(p: Probe) -> None:
    """S4 CONTROL: identical prompt, NO switch. Establishes the baseline can_use_tool
    count under `default`, without which S2's "zero prompts" proves nothing about
    whether the switch took effect."""
    await p.spawn("default")
    reader = asyncio.create_task(p.read_stdout())
    errs = asyncio.create_task(p.read_stderr())
    await asyncio.sleep(2)
    await p.send_prompt(TOOL_PROMPT)
    for _ in range(180):
        if p.result_frame is not None:
            break
        await asyncio.sleep(1)
    await asyncio.sleep(3)
    reader.cancel()
    errs.cancel()


async def scenario_deep_midturn(p: Probe) -> None:
    """S5: switch only once the model is DEMONSTRABLY mid-generation (>=25 content
    deltas emitted), closing S2's gap where the switch landed right after
    message_start but before any content."""
    await p.spawn("default")
    reader = asyncio.create_task(p.read_stdout())
    errs = asyncio.create_task(p.read_stderr())
    await asyncio.sleep(2)
    await p.send_prompt(TOOL_PROMPT)
    try:
        await asyncio.wait_for(p.deep_in_stream.wait(), timeout=120)
    except asyncio.TimeoutError:
        p.errors.append(f"only {p.delta_count} deltas in 120s -- never reached mid-generation")
        reader.cancel()
        errs.cancel()
        return
    print(f"[turn] {p.delta_count} deltas streamed at t={p._stamp()}s; switching mid-generation",
          flush=True)
    await p.send_set_mode("acceptEdits")
    for _ in range(180):
        if p.result_frame is not None:
            break
        await asyncio.sleep(1)
    await asyncio.sleep(3)
    reader.cancel()
    errs.cancel()


async def scenario_tighten_midturn(p: Probe) -> None:
    """S6 SAFETY DIRECTION: start in acceptEdits (Write auto-runs), then tighten to
    `default` mid-generation. If can_use_tool fires for the later Write, the gate is
    restored WITHIN the turn; if not, loosening is honoured mid-turn but tightening
    is not -- an asymmetry that matters far more than the loosening case."""
    await p.spawn("acceptEdits")
    reader = asyncio.create_task(p.read_stdout())
    errs = asyncio.create_task(p.read_stderr())
    await asyncio.sleep(2)
    await p.send_prompt(TOOL_PROMPT)
    try:
        await asyncio.wait_for(p.deep_in_stream.wait(), timeout=120)
    except asyncio.TimeoutError:
        p.errors.append(f"only {p.delta_count} deltas in 120s")
        reader.cancel()
        errs.cancel()
        return
    print(f"[turn] {p.delta_count} deltas streamed at t={p._stamp()}s; TIGHTENING to default",
          flush=True)
    await p.send_set_mode("default")
    for _ in range(180):
        if p.result_frame is not None:
            break
        await asyncio.sleep(1)
    await asyncio.sleep(3)
    reader.cancel()
    errs.cancel()


SCENARIOS = {
    "s1": scenario_idle,
    "s2": scenario_midturn,
    "s3": scenario_plan_exit,
    "s4": scenario_control_no_switch,
    "s5": scenario_deep_midturn,
    "s6": scenario_tighten_midturn,
}


async def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--scenario", default="all", choices=[*SCENARIOS, "all"])
    ap.add_argument("--outdir", default="/tmp/claude-mode-capture")
    ap.add_argument("--claude-bin", default=shutil.which("claude") or "claude")
    args = ap.parse_args()

    outdir = Path(args.outdir)
    outdir.mkdir(parents=True, exist_ok=True)
    workdir = outdir / "scratch"
    workdir.mkdir(exist_ok=True)

    names = list(SCENARIOS) if args.scenario == "all" else [args.scenario]
    summary = {}
    for name in names:
        print(f"\n{'=' * 60}\n{name}\n{'=' * 60}", flush=True)
        p = Probe(args.claude_bin, workdir, outdir, name)
        try:
            await SCENARIOS[name](p)
        except Exception as exc:  # keep partial capture on failure
            p.errors.append(f"{type(exc).__name__}: {exc}")
        summary[name] = await p.finish()
        print(json.dumps(summary[name], indent=2), flush=True)

    (outdir / "summary.json").write_text(json.dumps(summary, indent=2))
    print(f"\nCaptures written to {outdir}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))

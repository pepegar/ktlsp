#!/usr/bin/env python3

"""Run a protocol-level Kotlin language-server comparison probe.

The probe deliberately uses only standard LSP messages. It is intended for comparing
servers with different project-import and indexing architectures without relying on a
server-specific "ready" log line.
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import shlex
import statistics
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


JsonObject = dict[str, Any]


def uri_for(path: Path) -> str:
    return path.resolve().as_uri()


def token_position(text: str, token: str, occurrence: int) -> tuple[int, int, int]:
    if occurrence < 1:
        raise ValueError("occurrence must be at least 1")
    offset = -1
    for _ in range(occurrence):
        offset = text.find(token, offset + 1)
        if offset < 0:
            raise ValueError(f"could not find occurrence {occurrence} of {token!r}")
    line = text.count("\n", 0, offset)
    previous_newline = text.rfind("\n", 0, offset)
    character = offset if previous_newline < 0 else offset - previous_newline - 1
    return line, character, offset


class JsonRpcReader(threading.Thread):
    def __init__(self, stream: Any, messages: queue.Queue[tuple[float, JsonObject]]):
        super().__init__(daemon=True)
        self.stream = stream
        self.messages = messages

    def run(self) -> None:
        try:
            while True:
                headers: dict[str, str] = {}
                while True:
                    line = self.stream.readline()
                    if not line:
                        return
                    if line in (b"\r\n", b"\n"):
                        break
                    key, value = line.decode("utf-8", errors="replace").split(":", 1)
                    headers[key.strip().lower()] = value.strip()
                length = int(headers.get("content-length", "0"))
                if length <= 0:
                    continue
                payload = self.stream.read(length)
                if not payload:
                    return
                message = json.loads(payload.decode("utf-8"))
                self.messages.put((time.monotonic(), message))
        except Exception as exc:  # pragma: no cover - exercised only on broken servers
            self.messages.put((time.monotonic(), {"reader_error": repr(exc)}))


class ProcessTreeSampler(threading.Thread):
    def __init__(self, root_pid: int):
        super().__init__(daemon=True)
        self.root_pid = root_pid
        self.stop_event = threading.Event()
        self.samples_kib: list[int] = []

    def run(self) -> None:
        while not self.stop_event.is_set():
            self.samples_kib.append(self._sample())
            self.stop_event.wait(0.1)

    def stop(self) -> None:
        self.stop_event.set()
        self.join(timeout=2)

    def _sample(self) -> int:
        try:
            output = subprocess.check_output(
                ["ps", "-axo", "pid=,ppid=,rss="],
                text=True,
                stderr=subprocess.DEVNULL,
            )
        except (OSError, subprocess.SubprocessError):
            return 0
        rows: dict[int, tuple[int, int]] = {}
        for line in output.splitlines():
            parts = line.split()
            if len(parts) != 3:
                continue
            pid, parent_pid, rss_kib = (int(part) for part in parts)
            rows[pid] = (parent_pid, rss_kib)
        descendants = {self.root_pid}
        changed = True
        while changed:
            changed = False
            for pid, (parent_pid, _) in rows.items():
                if parent_pid in descendants and pid not in descendants:
                    descendants.add(pid)
                    changed = True
        return sum(rows.get(pid, (0, 0))[1] for pid in descendants)


@dataclass
class Response:
    elapsed_ms: float
    message: JsonObject


class LspSession:
    def __init__(
        self,
        command: list[str],
        root: Path,
        environment: dict[str, str],
        timeout_s: float,
    ):
        self.root = root
        self.timeout_s = timeout_s
        self.process = subprocess.Popen(
            command,
            cwd=root,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        assert self.process.stdin is not None
        assert self.process.stdout is not None
        assert self.process.stderr is not None
        self.start_time = time.monotonic()
        self.messages: queue.Queue[tuple[float, JsonObject]] = queue.Queue()
        self.reader = JsonRpcReader(self.process.stdout, self.messages)
        self.reader.start()
        self.stderr_lines: list[str] = []
        self.stderr_reader = threading.Thread(target=self._read_stderr, daemon=True)
        self.stderr_reader.start()
        self.sampler = ProcessTreeSampler(self.process.pid)
        self.sampler.start()
        self.next_id = 1
        self.notifications: list[dict[str, Any]] = []
        self.progress: list[dict[str, Any]] = []
        self.workspace_folders = [{"uri": uri_for(root), "name": root.name}]

    def _read_stderr(self) -> None:
        assert self.process.stderr is not None
        for line in self.process.stderr:
            self.stderr_lines.append(line.decode("utf-8", errors="replace").rstrip())

    def send(self, payload: JsonObject) -> None:
        assert self.process.stdin is not None
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.process.stdin.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
        self.process.stdin.write(body)
        self.process.stdin.flush()

    def notify(self, method: str, params: Any) -> None:
        self.send({"jsonrpc": "2.0", "method": method, "params": params})

    def request(self, method: str, params: Any, timeout_s: float | None = None) -> Response:
        request_id = self.next_id
        self.next_id += 1
        sent_at = time.monotonic()
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        deadline = sent_at + (timeout_s or self.timeout_s)
        while time.monotonic() < deadline:
            remaining = max(0.0, deadline - time.monotonic())
            try:
                timestamp, message = self.messages.get(timeout=min(0.2, remaining))
            except queue.Empty:
                return_code = self.process.poll()
                if return_code is not None:
                    raise RuntimeError(f"server exited with status {return_code} while waiting for {method}")
                continue
            if message.get("id") == request_id and ("result" in message or "error" in message):
                return Response(round((timestamp - sent_at) * 1000, 1), message)
            self._handle_incoming(timestamp, message)
        raise TimeoutError(f"timed out waiting for {method} after {timeout_s or self.timeout_s}s")

    def pump(self, duration_s: float) -> None:
        deadline = time.monotonic() + duration_s
        while time.monotonic() < deadline:
            try:
                timestamp, message = self.messages.get(timeout=min(0.1, deadline - time.monotonic()))
            except queue.Empty:
                continue
            self._handle_incoming(timestamp, message)

    def _handle_incoming(self, timestamp: float, message: JsonObject) -> None:
        method = message.get("method")
        if method is None:
            return
        relative_ms = round((timestamp - self.start_time) * 1000, 1)
        if "id" in message:
            self._answer_server_request(message)
            return
        params = message.get("params")
        if method == "$/progress":
            self.progress.append({"t_ms": relative_ms, "params": params})
        else:
            self.notifications.append({"t_ms": relative_ms, "method": method, "params": params})

    def _answer_server_request(self, message: JsonObject) -> None:
        method = message.get("method")
        params = message.get("params", {})
        if method == "workspace/configuration":
            result: Any = [{} for _ in params.get("items", [])]
        elif method == "workspace/workspaceFolders":
            result = self.workspace_folders
        elif method == "workspace/applyEdit":
            result = {"applied": False, "failureReason": "comparison probe does not mutate projects"}
        elif method == "window/showDocument":
            result = {"success": False}
        else:
            result = None
        self.send({"jsonrpc": "2.0", "id": message["id"], "result": result})

    def shutdown(self) -> None:
        try:
            self.request("shutdown", None, timeout_s=15)
            self.notify("exit", None)
        except (BrokenPipeError, OSError, RuntimeError, TimeoutError):
            pass
        try:
            self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.sampler.stop()


def meaningful_result(message: JsonObject) -> bool:
    if "error" in message:
        return False
    result = message.get("result")
    if result is None:
        return False
    if isinstance(result, (str, list)):
        return bool(result)
    if isinstance(result, dict):
        if isinstance(result.get("items"), list):
            return bool(result["items"])
        if isinstance(result.get("data"), list):
            return bool(result["data"])
        return bool(result)
    return True


def result_uris(message: JsonObject) -> list[str]:
    result = message.get("result")
    entries = result if isinstance(result, list) else [result]
    uris: list[str] = []
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        uri = entry.get("uri") or entry.get("targetUri")
        if isinstance(uri, str):
            uris.append(uri)
    return uris


def result_count(message: JsonObject) -> int | None:
    if "error" in message:
        return None
    result = message.get("result")
    if isinstance(result, list):
        return len(result)
    if isinstance(result, dict):
        if isinstance(result.get("items"), list):
            return len(result["items"])
        if isinstance(result.get("data"), list):
            return len(result["data"]) // 5
        return len(result)
    if result is None:
        return 0
    return 1


def summarize_response(response: Response) -> dict[str, Any]:
    message = response.message
    summary: dict[str, Any] = {
        "latency_ms": response.elapsed_ms,
        "ok": "error" not in message,
        "nonempty": meaningful_result(message),
        "result_count": result_count(message),
    }
    if "error" in message:
        summary["error"] = message["error"]
    uris = result_uris(message)
    if uris:
        summary["uris"] = uris
    return summary


def position_params(probe_uri: str, position: dict[str, int]) -> dict[str, Any]:
    return {"textDocument": {"uri": probe_uri}, "position": position}


def run_probe(args: argparse.Namespace) -> dict[str, Any]:
    root = args.root.resolve()
    probe_file = (root / args.probe_file).resolve()
    if not probe_file.is_relative_to(root):
        raise ValueError("probe file must be inside the project root")
    text = probe_file.read_text()
    expected_definition_uris = [
        uri_for((root / expected_file).resolve()) for expected_file in args.expected_definition_file
    ]
    line, character, offset = token_position(text, args.token, args.occurrence)
    position = {"line": line, "character": character + min(1, len(args.token) - 1)}
    completion_position = {"line": line, "character": character + min(3, len(args.token))}
    token_end_position = {"line": line, "character": character + len(args.token)}
    call_offset = text.find("(", offset + len(args.token))
    if call_offset >= 0 and "\n" not in text[offset:call_offset]:
        call_line = text.count("\n", 0, call_offset)
        call_previous_newline = text.rfind("\n", 0, call_offset)
        call_character = call_offset if call_previous_newline < 0 else call_offset - call_previous_newline - 1
        signature_position = {"line": call_line, "character": call_character + 1}
    else:
        signature_position = completion_position

    command = shlex.split(args.command)
    if "/" in command[0] and not Path(command[0]).is_absolute():
        command[0] = str(Path(command[0]).resolve())
    cache_dir = args.cache_dir.resolve()
    cache_dir.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment.update(
        {
            "GRADLE_USER_HOME": str(args.gradle_user_home.resolve()),
            "XDG_CACHE_HOME": str(cache_dir / "xdg-cache"),
            "XDG_CONFIG_HOME": str(cache_dir / "xdg-config"),
            "XDG_DATA_HOME": str(cache_dir / "xdg-data"),
        }
    )
    if args.java_home is not None:
        environment["JAVA_HOME"] = str(args.java_home.resolve())
    initialization_options: dict[str, Any] = {}
    if args.server_kind == "ktlsp":
        environment["KTLSP_CACHE_DIR"] = str(cache_dir / "ktlsp")
        initialization_options = {
            "diagnostics": {"scope": "openFilesOnly"},
            "compile_diagnostics": {"enabled": False},
        }
    elif args.server_kind == "jetbrains":
        command.extend(["--stdio", "--system-path", str(cache_dir / "system")])

    args.gradle_user_home.resolve().mkdir(parents=True, exist_ok=True)
    session = LspSession(command, root, environment, args.timeout)
    result: dict[str, Any] = {
        "server": args.name,
        "server_kind": args.server_kind,
        "command": command,
        "root": str(root),
        "probe_file": str(probe_file.relative_to(root)),
        "token": args.token,
        "occurrence": args.occurrence,
        "position": position,
    }
    try:
        initialize_params = {
            "processId": os.getpid(),
            "clientInfo": {"name": "ktlsp-comparison-probe", "version": "1"},
            "rootUri": uri_for(root),
            "workspaceFolders": session.workspace_folders,
            "capabilities": {
                "workspace": {
                    "configuration": True,
                    "workspaceFolders": True,
                    "symbol": {"dynamicRegistration": True},
                    "diagnostics": {"refreshSupport": True},
                },
                "window": {"workDoneProgress": True},
                "textDocument": {
                    "definition": {"linkSupport": True},
                    "typeDefinition": {"linkSupport": True},
                    "implementation": {"linkSupport": True},
                    "completion": {
                        "completionItem": {
                            "snippetSupport": True,
                            "documentationFormat": ["markdown", "plaintext"],
                        }
                    },
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": True},
                    "semanticTokens": {
                        "requests": {"full": True},
                        "tokenTypes": [
                            "namespace", "type", "class", "enum", "interface", "struct",
                            "typeParameter", "parameter", "variable", "property", "enumMember",
                            "event", "function", "method", "macro", "keyword", "modifier",
                            "comment", "string", "number", "regexp", "operator", "decorator",
                        ],
                        "tokenModifiers": [
                            "declaration", "definition", "readonly", "static", "deprecated",
                            "abstract", "async", "modification", "documentation", "defaultLibrary",
                        ],
                        "formats": ["relative"],
                    },
                    "inlayHint": {"dynamicRegistration": True},
                    "diagnostic": {"dynamicRegistration": True},
                },
            },
            "initializationOptions": initialization_options,
        }
        initialize = session.request("initialize", initialize_params)
        result["initialize_ms"] = round((time.monotonic() - session.start_time) * 1000, 1)
        result["initialize_response_ms"] = initialize.elapsed_ms
        result["initialize_ok"] = "error" not in initialize.message
        result["server_info"] = initialize.message.get("result", {}).get("serverInfo")
        result["capabilities"] = initialize.message.get("result", {}).get("capabilities", {})
        session.notify("initialized", {})
        session.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": uri_for(probe_file),
                    "languageId": "kotlin",
                    "version": 1,
                    "text": text,
                }
            },
        )

        probe_uri = uri_for(probe_file)
        definition_params = position_params(probe_uri, position)
        readiness_started = time.monotonic()
        readiness_timeout = args.readiness_timeout or args.timeout
        readiness_attempts: list[dict[str, Any]] = []
        ready_response: Response | None = None
        while time.monotonic() - readiness_started < readiness_timeout:
            remaining = readiness_timeout - (time.monotonic() - readiness_started)
            try:
                response = session.request(
                    "textDocument/definition",
                    definition_params,
                    timeout_s=max(1.0, remaining),
                )
            except (BrokenPipeError, OSError, RuntimeError, TimeoutError):
                break
            readiness_attempts.append(summarize_response(response))
            response_uris = result_uris(response.message)
            target_matches = not expected_definition_uris or any(
                expected_uri in response_uris for expected_uri in expected_definition_uris
            )
            if meaningful_result(response.message) and target_matches:
                ready_response = response
                break
            session.pump(min(0.5, max(0.0, remaining)))
        result["readiness_attempts"] = readiness_attempts
        result["ready_ms"] = (
            round((time.monotonic() - session.start_time) * 1000, 1)
            if ready_response is not None
            else None
        )
        result["ready"] = ready_response is not None
        result["definition_uris"] = result_uris(ready_response.message) if ready_response else []
        result["definition_target_ok"] = (
            any(expected_uri in result["definition_uris"] for expected_uri in expected_definition_uris)
            if expected_definition_uris
            else None
        )
        result["expected_definition_uris"] = expected_definition_uris

        text_lines = text.split("\n")
        full_range = {
            "start": {"line": 0, "character": 0},
            "end": {"line": len(text_lines) - 1, "character": len(text_lines[-1])},
        }
        requests: list[tuple[str, str, dict[str, Any]]] = [
            ("definition", "textDocument/definition", definition_params),
            ("hover", "textDocument/hover", position_params(probe_uri, position)),
            (
                "document_highlight",
                "textDocument/documentHighlight",
                position_params(probe_uri, position),
            ),
            (
                "completion",
                "textDocument/completion",
                {
                    **position_params(probe_uri, completion_position),
                    "context": {"triggerKind": 1},
                },
            ),
            (
                "references",
                "textDocument/references",
                {
                    **position_params(probe_uri, position),
                    "context": {"includeDeclaration": True},
                },
            ),
            ("document_symbol", "textDocument/documentSymbol", {"textDocument": {"uri": probe_uri}}),
            ("workspace_symbol", "workspace/symbol", {"query": args.token}),
            ("semantic_tokens", "textDocument/semanticTokens/full", {"textDocument": {"uri": probe_uri}}),
            ("folding_range", "textDocument/foldingRange", {"textDocument": {"uri": probe_uri}}),
            (
                "selection_range",
                "textDocument/selectionRange",
                {"textDocument": {"uri": probe_uri}, "positions": [position]},
            ),
            ("type_definition", "textDocument/typeDefinition", position_params(probe_uri, position)),
            ("implementation", "textDocument/implementation", position_params(probe_uri, position)),
            ("call_hierarchy", "textDocument/prepareCallHierarchy", position_params(probe_uri, position)),
            ("type_hierarchy", "textDocument/prepareTypeHierarchy", position_params(probe_uri, position)),
            ("prepare_rename", "textDocument/prepareRename", position_params(probe_uri, position)),
            (
                "rename",
                "textDocument/rename",
                {**position_params(probe_uri, position), "newName": f"{args.token}ComparisonProbe"},
            ),
            ("signature_help", "textDocument/signatureHelp", position_params(probe_uri, signature_position)),
            (
                "inlay_hint",
                "textDocument/inlayHint",
                {"textDocument": {"uri": probe_uri}, "range": full_range},
            ),
            (
                "formatting",
                "textDocument/formatting",
                {"textDocument": {"uri": probe_uri}, "options": {"tabSize": 4, "insertSpaces": True}},
            ),
            (
                "code_action",
                "textDocument/codeAction",
                {
                    "textDocument": {"uri": probe_uri},
                    "range": {"start": position, "end": token_end_position},
                    "context": {"diagnostics": []},
                },
            ),
            (
                "document_diagnostic",
                "textDocument/diagnostic",
                {"textDocument": {"uri": probe_uri}},
            ),
        ]
        feature_results: dict[str, Any] = {}
        for feature, method, params in requests:
            samples: list[dict[str, Any]] = []
            sample_count = args.request_samples if ready_response is not None else 1
            for _ in range(sample_count):
                try:
                    samples.append(summarize_response(session.request(method, params, timeout_s=30)))
                except (BrokenPipeError, OSError, RuntimeError, TimeoutError) as exc:
                    samples.append({"ok": False, "nonempty": False, "error": str(exc)})
                    break
            latency_values = [sample["latency_ms"] for sample in samples if "latency_ms" in sample]
            feature_results[feature] = {
                "method": method,
                "samples": samples,
                "median_ms": round(statistics.median(latency_values), 1) if latency_values else None,
                "ok": any(sample.get("ok") for sample in samples),
                "nonempty": any(sample.get("nonempty") for sample in samples),
            }
        result["features"] = feature_results
        if args.wait_progress_token:
            progress_deadline = time.monotonic() + args.wait_progress_timeout
            while time.monotonic() < progress_deadline:
                progress_finished = any(
                    event.get("params", {}).get("token") == args.wait_progress_token
                    and event.get("params", {}).get("value", {}).get("kind") == "end"
                    for event in session.progress
                )
                if progress_finished or session.process.poll() is not None:
                    break
                session.pump(min(0.25, progress_deadline - time.monotonic()))
            else:
                progress_finished = False
            result["index_complete"] = progress_finished
            result["index_complete_ms"] = (
                round((time.monotonic() - session.start_time) * 1000, 1) if progress_finished else None
            )
            result["rss_after_index_mib"] = (
                round(session.sampler.samples_kib[-1] / 1024, 1) if session.sampler.samples_kib else None
            )
        session.pump(1.0)
        result["rss_after_probe_mib"] = (
            round(session.sampler.samples_kib[-1] / 1024, 1) if session.sampler.samples_kib else None
        )
    except Exception as exc:  # Preserve failed-start evidence in the JSON artifact.
        result["fatal_error"] = repr(exc)
    finally:
        session.shutdown()
        result["peak_rss_mib"] = (
            round(max(session.sampler.samples_kib, default=0) / 1024, 1)
            if session.sampler.samples_kib
            else None
        )
        result["process_exit_code"] = session.process.returncode
        result["notifications"] = session.notifications
        result["progress"] = session.progress
        result["stderr_tail"] = session.stderr_lines[-80:]
    return result


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--name", required=True)
    parser.add_argument("--server-kind", required=True, choices=["ktlsp", "jetbrains"])
    parser.add_argument("--command", required=True, help="shell-style command containing the server executable")
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--probe-file", required=True, type=Path)
    parser.add_argument("--token", required=True)
    parser.add_argument("--occurrence", type=int, default=1)
    parser.add_argument(
        "--expected-definition-file",
        type=Path,
        action="append",
        default=[],
        help="valid target path relative to the root; repeat for expect/actual alternatives",
    )
    parser.add_argument("--cache-dir", required=True, type=Path)
    parser.add_argument("--gradle-user-home", required=True, type=Path)
    parser.add_argument("--java-home", type=Path)
    parser.add_argument("--timeout", type=float, default=180.0)
    parser.add_argument("--readiness-timeout", type=float)
    parser.add_argument("--request-samples", type=int, default=3)
    parser.add_argument("--wait-progress-token")
    parser.add_argument("--wait-progress-timeout", type=float, default=120.0)
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    result = run_probe(args)
    rendered = json.dumps(result, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered)
    else:
        sys.stdout.write(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

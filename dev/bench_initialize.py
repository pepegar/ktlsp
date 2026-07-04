#!/usr/bin/env python3

import argparse
import json
import os
import queue
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple


def uri_for(path: Path) -> str:
    return path.resolve().as_uri()


class JsonRpcReader(threading.Thread):
    def __init__(self, stream, out_queue: "queue.Queue[Tuple[float, Dict[str, Any]]]"):
        super().__init__(daemon=True)
        self.stream = stream
        self.out_queue = out_queue

    def run(self) -> None:
        try:
            while True:
                headers = {}
                while True:
                    line = self.stream.readline()
                    if not line:
                        return
                    if line == b"\r\n":
                        break
                    key, value = line.decode("utf-8").split(":", 1)
                    headers[key.strip().lower()] = value.strip()
                length = int(headers.get("content-length", "0"))
                if length <= 0:
                    continue
                payload = self.stream.read(length)
                if not payload:
                    return
                message = json.loads(payload.decode("utf-8"))
                self.out_queue.put((time.monotonic(), message))
        except Exception as exc:
            self.out_queue.put((time.monotonic(), {"reader_error": str(exc)}))


def send_message(proc: subprocess.Popen[bytes], payload: dict[str, Any]) -> None:
    body = json.dumps(payload).encode("utf-8")
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8")
    assert proc.stdin is not None
    proc.stdin.write(header)
    proc.stdin.write(body)
    proc.stdin.flush()


def wait_for_message(
    messages: "queue.Queue[Tuple[float, Dict[str, Any]]]",
    predicate,
    timeout_s: float,
    stash: List[Tuple[float, Dict[str, Any]]],
):
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        remaining = max(0.0, deadline - time.monotonic())
        try:
            item = messages.get(timeout=remaining)
        except queue.Empty:
            break
        ts, msg = item
        stash.append(item)
        if predicate(msg):
            return ts, msg
    return None, None


def benchmark(
    binary: Path,
    root: Path,
    probe_file: Optional[Path],
    timeout_s: float,
    request_document_diagnostic: bool,
    request_workspace_diagnostic: bool,
    diagnostics_scope: str,
) -> Dict[str, Any]:
    proc = subprocess.Popen(
        [str(binary)],
        cwd=str(root),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert proc.stdout is not None
    assert proc.stderr is not None
    messages: "queue.Queue[Tuple[float, Dict[str, Any]]]" = queue.Queue()
    reader = JsonRpcReader(proc.stdout, messages)
    reader.start()
    stderr_lines: List[str] = []

    def read_stderr():
        for line in proc.stderr:
            stderr_lines.append(line.decode("utf-8", errors="replace").rstrip())

    stderr_thread = threading.Thread(target=read_stderr, daemon=True)
    stderr_thread.start()

    start = time.monotonic()
    init_id = 1
    send_message(
        proc,
        {
            "jsonrpc": "2.0",
            "id": init_id,
            "method": "initialize",
            "params": {
                "processId": os.getpid(),
                "rootUri": uri_for(root),
                "capabilities": {
                    "window": {"workDoneProgress": True},
                },
                "initializationOptions": {
                    "diagnostics": {"scope": diagnostics_scope},
                    "compile_diagnostics": {"enabled": False},
                },
                "workspaceFolders": [{"uri": uri_for(root), "name": root.name}],
            },
        },
    )

    stash: List[Tuple[float, Dict[str, Any]]] = []
    init_ts, init_msg = wait_for_message(
        messages,
        lambda msg: msg.get("id") == init_id,
        timeout_s,
        stash,
    )
    if init_msg is None:
        proc.kill()
        raise RuntimeError("initialize response timeout")

    send_message(proc, {"jsonrpc": "2.0", "method": "initialized", "params": {}})

    did_open_sent_at = None
    semantic_tokens_id = 2
    document_diagnostic_id = 3
    workspace_diagnostic_id = 4
    semantic_response_ts = None
    semantic_response = None
    document_diagnostic_response_ts = None
    document_diagnostic_response = None
    workspace_diagnostic_response_ts = None
    workspace_diagnostic_response = None
    if probe_file is not None:
        text = probe_file.read_text()
        send_message(
            proc,
            {
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri_for(probe_file),
                        "languageId": "kotlin",
                        "version": 1,
                        "text": text,
                    }
                },
            },
        )
        did_open_sent_at = time.monotonic()
        send_message(
            proc,
            {
                "jsonrpc": "2.0",
                "id": semantic_tokens_id,
                "method": "textDocument/semanticTokens/full",
                "params": {"textDocument": {"uri": uri_for(probe_file)}},
            },
        )
        if request_document_diagnostic:
            send_message(
                proc,
                {
                    "jsonrpc": "2.0",
                    "id": document_diagnostic_id,
                    "method": "textDocument/diagnostic",
                    "params": {
                        "textDocument": {"uri": uri_for(probe_file)},
                    },
                },
            )
    if request_workspace_diagnostic:
        send_message(
            proc,
            {
                "jsonrpc": "2.0",
                "id": workspace_diagnostic_id,
                "method": "workspace/diagnostic",
                "params": {
                    "previousResultIds": [],
                },
            },
        )

    project_index_ts = None
    library_index_ts = None
    progress_events = []
    log_messages = []

    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        remaining = max(0.0, deadline - time.monotonic())
        try:
            ts, msg = messages.get(timeout=remaining)
        except queue.Empty:
            break
        stash.append((ts, msg))
        if msg.get("id") == semantic_tokens_id:
            semantic_response_ts = ts
            semantic_response = msg
        elif msg.get("id") == document_diagnostic_id:
            document_diagnostic_response_ts = ts
            document_diagnostic_response = msg
        elif msg.get("id") == workspace_diagnostic_id:
            workspace_diagnostic_response_ts = ts
            workspace_diagnostic_response = msg
        method = msg.get("method")
        if method == "window/logMessage":
            message = msg.get("params", {}).get("message", "")
            log_messages.append({"t_ms": round((ts - start) * 1000, 1), "message": message})
            if message.startswith("ktlsp indexed ") and "project files" in message and project_index_ts is None:
                project_index_ts = ts
            elif message.startswith("ktlsp indexed ") and "library files" in message and library_index_ts is None:
                library_index_ts = ts
        elif method == "$/progress":
            progress_events.append({"t_ms": round((ts - start) * 1000, 1), "params": msg.get("params")})
        if project_index_ts is not None and library_index_ts is not None and (
            probe_file is None or semantic_response_ts is not None
        ) and (
            not request_document_diagnostic or document_diagnostic_response_ts is not None
        ) and (
            not request_workspace_diagnostic or workspace_diagnostic_response_ts is not None
        ):
            break

    try:
        send_message(proc, {"jsonrpc": "2.0", "id": 99, "method": "shutdown", "params": None})
        send_message(proc, {"jsonrpc": "2.0", "method": "exit", "params": None})
    except BrokenPipeError:
        pass
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()

    result: Dict[str, Any] = {
        "binary": str(binary),
        "root": str(root),
        "initialize_ms": round((init_ts - start) * 1000, 1) if init_ts is not None else None,
        "project_index_ms": round((project_index_ts - start) * 1000, 1) if project_index_ts else None,
        "library_index_ms": round((library_index_ts - start) * 1000, 1) if library_index_ts else None,
        "progress_events": progress_events,
        "log_messages": log_messages,
        "stderr_tail": stderr_lines[-20:],
        "initialize_result_keys": sorted(init_msg.get("result", {}).keys()) if init_msg else None,
        "initialize_response": init_msg,
        "methods_seen": sorted(
            {
                msg["method"]
                for _, msg in stash
                if isinstance(msg, dict) and "method" in msg
            }
        ),
        "method_counts": {
            method: sum(1 for _, msg in stash if isinstance(msg, dict) and msg.get("method") == method)
            for method in sorted(
                {
                    msg["method"]
                    for _, msg in stash
                    if isinstance(msg, dict) and "method" in msg
                }
            )
        },
    }
    if probe_file is not None:
        result["probe_file"] = str(probe_file)
        result["semantic_tokens_ms"] = (
            round((semantic_response_ts - did_open_sent_at) * 1000, 1)
            if semantic_response_ts is not None and did_open_sent_at is not None
            else None
        )
        if semantic_response is not None:
            data = semantic_response.get("result", {}).get("data")
            result["semantic_tokens_count"] = len(data) // 5 if isinstance(data, list) else None
            result["semantic_ok"] = "result" in semantic_response and "error" not in semantic_response
    if request_document_diagnostic:
        result["document_diagnostic_ms"] = (
            round((document_diagnostic_response_ts - did_open_sent_at) * 1000, 1)
            if document_diagnostic_response_ts is not None and did_open_sent_at is not None
            else None
        )
        if document_diagnostic_response is not None:
            items = (
                document_diagnostic_response.get("result", {})
                .get("fullDocumentDiagnosticReport", {})
                .get("items")
            )
            if items is None:
                items = (
                    document_diagnostic_response.get("result", {})
                    .get("Full", {})
                    .get("fullDocumentDiagnosticReport", {})
                    .get("items")
                )
            result["document_diagnostic_ok"] = (
                "result" in document_diagnostic_response and "error" not in document_diagnostic_response
            )
            result["document_diagnostic_items"] = len(items) if isinstance(items, list) else None
            if "error" in document_diagnostic_response:
                result["document_diagnostic_error"] = document_diagnostic_response["error"]
    if request_workspace_diagnostic:
        result["workspace_diagnostic_ms"] = (
            round((workspace_diagnostic_response_ts - start) * 1000, 1)
            if workspace_diagnostic_response_ts is not None
            else None
        )
        if workspace_diagnostic_response is not None:
            items = workspace_diagnostic_response.get("result", {}).get("items")
            result["workspace_diagnostic_ok"] = (
                "result" in workspace_diagnostic_response and "error" not in workspace_diagnostic_response
            )
            result["workspace_diagnostic_items"] = len(items) if isinstance(items, list) else None
            if "error" in workspace_diagnostic_response:
                result["workspace_diagnostic_error"] = workspace_diagnostic_response["error"]
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description="Benchmark ktlsp initialize/indexing against a workspace root")
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--probe-file", type=Path)
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument("--document-diagnostic", action="store_true")
    parser.add_argument("--workspace-diagnostic", action="store_true")
    parser.add_argument(
        "--diagnostics-scope",
        choices=["openFilesOnly", "workspace"],
        default="openFilesOnly",
    )
    args = parser.parse_args()

    result = benchmark(
        args.binary,
        args.root,
        args.probe_file,
        args.timeout,
        args.document_diagnostic,
        args.workspace_diagnostic,
        args.diagnostics_scope,
    )
    json.dump(result, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

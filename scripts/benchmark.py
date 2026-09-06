#!/usr/bin/env python3
"""Reproducible Windows stress test. Only generated files inside a new run are mutated.

Python 3.11+, no third-party packages. GB is decimal. JSON stdout from Vault stays private;
the exported metrics contain no local paths, session IDs or conversation text.
"""
import argparse
import base64
import ctypes as ct
from ctypes import wintypes as wt
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import shutil
import subprocess
import threading
import time

GB = 1_000_000_000
MARKER = "codex-vault-generated-benchmark-v1"
PROBE = "Historical benchmark sentinel: café 🦀 rotating refresh tokens."
STAMP = "2026-01-01T00:00:00.000Z"


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def bytes_under(path):
    return sum(p.stat().st_size for p in path.rglob("*") if p.is_file()) if path.exists() else 0


def storage(session, vault):
    native = session.stat().st_size if session.exists() else 0
    total_vault = bytes_under(vault)
    return dict(native_bytes=native, vault_bytes=total_vault,
                backup_bytes=bytes_under(vault / "backups"),
                index_bytes=sum(p.stat().st_size for p in vault.glob("index.sqlite*") if p.is_file()),
                total_bytes=native + total_vault)


def windows_counters(process):
    class Memory(ct.Structure):
        _fields_ = [("cb", wt.DWORD), ("faults", wt.DWORD)] + [
            (name, ct.c_size_t) for name in ["peak", "working", "peak_paged", "paged",
                                           "peak_nonpaged", "nonpaged", "pagefile", "peak_pagefile"]]

    class IO(ct.Structure):
        _fields_ = [(name, ct.c_ulonglong) for name in ["reads", "writes", "other", "read_bytes", "write_bytes", "other_bytes"]]

    memory, io = Memory(), IO()
    memory.cb = ct.sizeof(memory)
    handle = wt.HANDLE(int(process._handle))
    psapi = ct.WinDLL("psapi", use_last_error=True)
    kernel = ct.WinDLL("kernel32", use_last_error=True)
    if not psapi.GetProcessMemoryInfo(handle, ct.byref(memory), memory.cb):
        raise ct.WinError(ct.get_last_error())
    if not kernel.GetProcessIoCounters(handle, ct.byref(io)):
        raise ct.WinError(ct.get_last_error())
    return dict(peak_ram_bytes=memory.peak, approximate_read_bytes=io.read_bytes,
                approximate_write_bytes=io.write_bytes)


def hardware():
    import winreg

    class MemoryStatus(ct.Structure):
        _fields_ = [("length", wt.DWORD), ("load", wt.DWORD)] + [
            (name, ct.c_ulonglong) for name in ["total", "available", "page_total", "page_available",
                                              "virtual_total", "virtual_available", "extended_available"]]

    status = MemoryStatus()
    status.length = ct.sizeof(status)
    if not ct.WinDLL("kernel32", use_last_error=True).GlobalMemoryStatusEx(ct.byref(status)):
        raise ct.WinError(ct.get_last_error())
    with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0") as key:
        cpu = winreg.QueryValueEx(key, "ProcessorNameString")[0].strip()
    return dict(os=platform.platform(), machine=platform.machine(), logical_cpus=os.cpu_count(),
                cpu=cpu, physical_memory_bytes=status.total)


class Runner:
    def __init__(self, binary, root, session):
        self.binary, self.root, self.session = binary, root, session
        self.vault = root / "vault"
        self.env = dict(os.environ, CODEX_HOME=str(root / "codex"), CODEX_VAULT_HOME=str(self.vault))
        # Avoid probing an unrelated PATH Codex. The fixture declares the writer's version.
        self.env["CODEX_VAULT_CODEX_VERSION"] = "0.152.1"
        self.metrics = []

    def run(self, name, *args):
        before = storage(self.session, self.vault)
        peak_disk = [before["total_bytes"]]
        done = threading.Event()

        def sample_disk():
            while not done.wait(0.2):
                try:
                    peak_disk[0] = max(peak_disk[0], bytes_under(self.root / "codex") + bytes_under(self.vault))
                except FileNotFoundError:
                    pass  # an atomic replacement raced this approximate sample

        monitor = threading.Thread(target=sample_disk, daemon=True)
        stdout_path, stderr_path = self.root / f"{name}.stdout", self.root / f"{name}.stderr"
        started = time.perf_counter()
        monitor.start()
        try:
            with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
                process = subprocess.Popen([str(self.binary), "--json", "--no-progress", "--jobs", "1", *args],
                                           env=self.env, stdout=stdout, stderr=stderr, stdin=subprocess.DEVNULL)
                code = process.wait()
                counters = windows_counters(process)
        finally:
            done.set()
            monitor.join()
        elapsed = time.perf_counter() - started
        after = storage(self.session, self.vault)
        entry = dict(operation=name, seconds=round(elapsed, 3), exit_code=code, **counters,
                     input_bytes=before["native_bytes"], output_bytes=after["native_bytes"],
                     backup_bytes=after["backup_bytes"], index_bytes=after["index_bytes"],
                     storage_before=before, storage_after=after,
                     sampled_peak_storage_bytes=max(peak_disk[0], after["total_bytes"]),
                     net_storage_delta_bytes=after["total_bytes"] - before["total_bytes"],
                     native_output_ratio=after["native_bytes"] / max(1, before["native_bytes"]))
        self.metrics.append(entry)
        (self.root / "operations.json").write_text(json.dumps(self.metrics, indent=2), encoding="utf-8")
        print(f"{name}: {elapsed:.2f}s, peak RAM {counters['peak_ram_bytes']/1e6:.1f} MB, exit {code}", flush=True)
        if code:
            raise RuntimeError(f"{name} failed; inspect private stdout/stderr in the run directory")
        return json.loads(stdout_path.read_text(encoding="utf-8-sig"))


def generate(path, target, seed):
    """Bounded-memory generator, deterministic for a fixed Python version/seed/size.

    Tool payloads combine fresh pseudorandom base64 (35%) and repeated diagnostic text
    (65%). Every 127th tool record is 2 MiB; others are 128 KiB. No individual record
    exceeds Vault's 16 MiB indexed-line limit. Three checkpoints, with a 1% live tail.
    """
    rng = random.Random(seed)
    sid = "11111111-1111-4111-8111-000000000001"
    project = "C:/synthetic/benchmark"
    path.parent.mkdir(parents=True, exist_ok=True)
    hasher = hashlib.sha256()
    count, size, turn_number, checkpoints = 0, 0, 0, 0
    with path.open("wb", buffering=1024 * 1024) as stream:
        def record(kind, payload):
            nonlocal count, size
            data = (json.dumps(dict(timestamp=STAMP, type=kind, payload=payload),
                               ensure_ascii=False, separators=(",", ":")) + "\n").encode()
            stream.write(data)
            hasher.update(data)
            size += len(data)
            count += 1

        record("session_meta", dict(id=sid, timestamp=STAMP, cwd=project, originator="codex_cli_rs",
                                    cli_version="0.152.1", source="cli", model_provider="mock",
                                    base_instructions=dict(text="Synthetic benchmark conversation.")))
        record("response_item", dict(type="message", role="assistant", content=[dict(type="output_text", text=PROBE)]))
        thresholds = [target * fraction for fraction in (0.35, 0.70, 0.99)]
        while size < target:
            if checkpoints < len(thresholds) and size >= thresholds[checkpoints]:
                record("compacted", dict(message="Synthetic checkpoint", window_number=3 + checkpoints,
                                         replacement_history=[dict(type="message", role="user", content=[dict(
                                             type="input_text", text="Continue the synthetic benchmark; previous checks passed.")])]))
                checkpoints += 1
            turn_id = f"turn-{turn_number}"
            text = f"Check synthetic module {turn_number}. Unicode: café 日本語 🦀."
            record("event_msg", dict(type="task_started", turn_id=turn_id, model_context_window=200000))
            record("turn_context", dict(turn_id=turn_id, cwd=project, approval_policy="never",
                                        sandbox_policy=dict(type="read-only"), model="gpt-5.4", effort="medium", summary="auto"))
            record("event_msg", dict(type="user_message", message=text, images=[], local_images=[], text_elements=[]))
            record("response_item", dict(type="message", role="user", content=[dict(type="input_text", text=text)]))
            record("response_item", dict(type="function_call", name="exec_command", call_id=turn_id,
                                          arguments=json.dumps(dict(cmd="synthetic diagnostic"))))
            length = 2 * 1024 * 1024 if turn_number % 127 == 0 else 128 * 1024
            entropy = base64.b64encode(rng.randbytes(int(length * 0.35 * 0.75))).decode("ascii")
            repeated = "Synthetic diagnostic: checked module; tests passed. café 🦀\n"
            payload = (repeated * ((length - len(entropy)) // len(repeated))) + entropy
            record("response_item", dict(type="function_call_output", call_id=turn_id, output=payload))
            record("response_item", dict(type="message", role="assistant", content=[dict(
                type="output_text", text=f"Module {turn_number} checked; keep rotating refresh tokens.")]))
            record("event_msg", dict(type="task_complete", turn_id=turn_id, last_agent_message="Synthetic checks passed."))
            turn_number += 1
    return dict(bytes=size, records=count, turns=turn_number, checkpoints=checkpoints, sha256=hasher.hexdigest())


def run_case(binary, root, gb, seed):
    root.mkdir()
    (root / ".generated-by-vault-benchmark").write_text(MARKER, encoding="ascii")
    session = root / "codex/sessions/2026/01/01/rollout-2026-01-01T00-00-00-11111111-1111-4111-8111-000000000001.jsonl"
    generated = generate(session, int(gb * GB), seed)
    if generated["checkpoints"] != 3:
        raise RuntimeError("Fixture too small for three checkpoints; use at least 0.02 GB")
    runner = Runner(binary, root, session)
    target = str(session)
    runner.run("scan", "scan")
    analysis = runner.run("analyze", "analyze", target)
    if not analysis["analysis"]["can_compact"]:
        raise RuntimeError("Generated fixture must be compactable")
    runner.run("archive", "archive", target)
    runner.run("doctor_archive_deep", "doctor", target, "--deep")
    preview = runner.run("compact_dry_run", "compact", target, "--dry-run")
    assert preview["status"] == "preview"
    compact = runner.run("compact", "compact", target)
    assert compact["status"] == "ok" and session.stat().st_size < generated["bytes"]
    compact_storage = storage(session, runner.vault)
    runner.run("doctor", "doctor", target)
    runner.run("doctor_compacted_deep", "doctor", target, "--deep")
    runner.run("index", "index")
    matches = runner.run("search", "search", "Historical benchmark sentinel")
    assert len(matches["matches"]) == 1
    passage = matches["matches"][0]["id"]
    read = runner.run("read", "read", passage)
    assert read["text"] == PROBE and read["verified_reference"]["kind"] == "backup"
    runner.run("restore", "restore", target, "--original")
    restored_hash = digest(session)
    assert restored_hash == generated["sha256"], "Restore hash mismatch"
    runner.run("doctor_final_deep", "doctor", target, "--deep")
    runner.run("index_restored", "index")
    assert runner.run("read_restored", "read", passage)["text"] == PROBE
    result = dict(requested_gb=gb, generated=generated, operations=runner.metrics, passed=True,
                  restored_sha256=restored_hash, compact_storage=compact_storage,
                  backup_compression_ratio=compact_storage["backup_bytes"] / generated["bytes"],
                  net_saved_bytes=generated["bytes"] - compact_storage["total_bytes"],
                  net_saved_percent=100 * (1 - compact_storage["total_bytes"] / generated["bytes"]))
    (root / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    return result


def clean_generated_case(root, run_root):
    resolved = root.resolve()
    if resolved.parent != run_root.resolve() or resolved.is_symlink():
        raise RuntimeError("Cleanup target outside this run")
    if (resolved / ".generated-by-vault-benchmark").read_text(encoding="ascii") != MARKER:
        raise RuntimeError("Missing generated-data ownership marker")
    # Keep reports and logs. Delete only the generator's isolated Codex/Vault trees.
    for name in ("codex", "vault"):
        target = resolved / name
        if target.is_symlink() or target.resolve().parent != resolved:
            raise RuntimeError("Unsafe generated-data cleanup target")
        shutil.rmtree(target)


def markdown(report):
    lines = ["# Multi-GB Windows benchmark", "", f"Vault `{report['vault_version']}`; decimal GB. Synthetic tool-heavy data.", "",
             "| Input | Compact | Peak RAM (all operations) | Net savings, backups included | Exact restore |",
             "| --- | ---: | ---: | ---: | --- |"]
    for case in report["cases"]:
        compact = next(o for o in case["operations"] if o["operation"] == "compact")
        peak = max(o["peak_ram_bytes"] for o in case["operations"])
        lines.append(f"| {case['requested_gb']:g} GB | {compact['seconds']:.2f} s | {peak/1e6:.1f} MB | {case['net_saved_percent']:.2f}% | PASS |")
    for case in report["cases"]:
        lines += ["", f"## {case['requested_gb']:g} GB", "", "| Operation | Seconds | Peak RAM (MB) | Read (GB) | Written (GB) |", "| --- | ---: | ---: | ---: | ---: |"]
        for op in case["operations"]:
            lines.append(f"| {op['operation']} | {op['seconds']:.3f} | {op['peak_ram_bytes']/1e6:.1f} | {op['approximate_read_bytes']/GB:.3f} | {op['approximate_write_bytes']/GB:.3f} |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--sizes-gb", nargs="+", type=float, default=[1, 5, 10], help="Decimal sizes; 20 is supported when disk space permits")
    parser.add_argument("--output", type=Path, default=Path("validation") / time.strftime("benchmark-%Y%m%d-%H%M%S"))
    parser.add_argument("--seed", type=int, default=20260906)
    parser.add_argument("--keep-data", action="store_true", help="Retain generated rollouts/backups after verification")
    args = parser.parse_args()
    if os.name != "nt":
        parser.error("This harness uses Windows process counters; run on Windows")
    if any(size < 0.02 or size > 20 for size in args.sizes_gb):
        parser.error("Sizes must be between 0.02 and 20 GB")
    binary = args.binary.resolve(strict=True)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    version = subprocess.check_output([str(binary), "--version"], text=True).strip()
    report = dict(schema_version=1, complete=False, requested_sizes_gb=args.sizes_gb,
                  vault_version=version, binary_sha256=digest(binary), system=hardware(),
                  generator=dict(seed=args.seed, python=platform.python_version(), entropy_fraction=0.35, live_tail_fraction=0.01),
                  measurement=dict(ram="Windows PeakWorkingSetSize of each CLI process; excludes generator and child processes",
                                   io="Windows process IO transfer counters: logical I/O, not physical device traffic",
                                   disk="Logical file lengths; temporary disk usage sampled every 200 ms",
                                   cache="Warm/OS-managed cache; no cache flush; sequential operations; one worker"), cases=[])
    for index, size in enumerate(args.sizes_gb):
        required = int(size * GB * 1.4 + 2 * GB)
        if shutil.disk_usage(output).free < required:
            raise RuntimeError(f"Need at least {required/GB:.1f} GB free for the next case (including a 2 GB reserve)")
        root = output / f"case-{index+1}"
        print(f"Starting {size:g} GB synthetic case", flush=True)
        report["cases"].append(run_case(binary, root, size, args.seed))
        (output / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
        (output / "report.md").write_text(markdown(report), encoding="utf-8")
        if not args.keep_data:
            clean_generated_case(root, output)
    report["complete"] = True
    (output / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print("All requested sizes verified; JSON and Markdown reports written.", flush=True)


if __name__ == "__main__":
    main()

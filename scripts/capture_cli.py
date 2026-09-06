#!/usr/bin/env python3
"""Capture real CLI output as SVG documentation previews, using synthetic data only.

These are text captures, not native Windows screenshots. No conversation content or
actual filesystem paths are copied into the public images.
"""
import argparse
import html
import json
import os
from pathlib import Path
import subprocess
import tempfile
import xml.etree.ElementTree as ET


def svg(command, output):
    lines = output.rstrip().splitlines()
    width = max(960, max(len(line) for line in lines) * 10 + 64)
    height = 130 + len(lines) * 23
    text = "\n".join(f'<text x="32" y="{112 + i * 23}">{html.escape(line)}</text>' for i, line in enumerate(lines))
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
<title>{html.escape(command)} — actual CLI output with synthetic data</title>
<desc>Rendered terminal output captured from the executable; not a native Windows screenshot.</desc>
<rect width="{width}" height="{height}" rx="12" fill="#101820"/>
<path d="M12 0 H{width-12} Q{width} 0 {width} 12 V48 H0 V12 Q0 0 12 0" fill="#1c2935"/>
<g fill="#91a4b5" font-family="Consolas,DejaVu Sans Mono,monospace" font-size="13"><text x="32" y="30">CODEX VAULT  /  SYNTHETIC DEMO</text></g>
<g font-family="Consolas,DejaVu Sans Mono,monospace" font-size="16" xml:space="preserve">
<text x="32" y="80" fill="#7ee6be">&gt; {html.escape(command)}</text>
<g fill="#e2eaf0">{text}</g></g></svg>
'''


def png(svg_text, destination):
    # Rasterize this script's small SVG vocabulary for sharing sites that reject SVG.
    # Pillow is an optional documentation dependency, never a Vault runtime dependency.
    from PIL import Image, ImageDraw, ImageFont
    root = ET.fromstring(svg_text)
    scale = 2
    width, height = int(root.attrib["width"]), int(root.attrib["height"])
    image = Image.new("RGB", (width * scale, height * scale), "#101820")
    draw = ImageDraw.Draw(image)
    draw.rectangle((0, 0, width * scale, 48 * scale), fill="#1c2935")
    font_path = Path(os.environ.get("SystemRoot", "C:/Windows")) / "Fonts/consola.ttf"
    if not font_path.exists():
        font_path = Path("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf")

    def render(node, inherited):
        style = dict(inherited, **node.attrib)
        if node.tag.endswith("}text"):
            font = ImageFont.truetype(str(font_path), int(style["font-size"]) * scale)
            draw.text((int(style["x"]) * scale, int(style["y"]) * scale), node.text or "",
                      fill=style.get("fill", "#e2eaf0"), font=font, anchor="ls")
        for child in node:
            render(child, style)

    render(root, {})
    image.save(destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", type=Path, default=Path("docs/assets"))
    parser.add_argument("--png", action="store_true", help="Also export PNG (requires Pillow and a monospace font)")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="vault-public-demo-") as temp:
        root = Path(temp)
        sessions = root / "codex/sessions"
        sessions.mkdir(parents=True)
        entries = []
        for i, title in enumerate(["Authentication service", "Desktop release checklist", "Documentation refresh"], 1):
            sid = f"demo-{i}"
            records = [dict(type="session_meta", payload=dict(id=sid, cwd="C:/Projects/sample-app", cli_version="0.152.1")),
                       dict(type="event_msg", payload=dict(type="user_message", message="Synthetic demo conversation.")),
                       dict(type="response_item", payload=dict(type="function_call_output", call_id="demo", output="Synthetic diagnostic. " * (50000 // i)))]
            (sessions / f"rollout-demo-{i}.jsonl").write_text("\n".join(json.dumps(r) for r in records) + "\n", encoding="utf-8")
            entries.append(dict(id=sid, thread_name=title, updated_at="2026-01-01T00:00:00Z"))
        (root / "codex/session_index.jsonl").write_text("\n".join(json.dumps(e) for e in entries) + "\n", encoding="utf-8")
        env = dict(os.environ, CODEX_HOME=str(root / "codex"), CODEX_VAULT_HOME=str(root / "vault"))
        for name, command, stdin in [("cli-menu", ["menu"], "q\n"), ("cli-help", ["--help"], "")]:
            result = subprocess.run([str(binary), *command], input=stdin.encode(), capture_output=True, env=env, check=True)
            output = result.stdout.decode("utf-8")
            assert not result.stderr, "Unexpected diagnostics in the demo"
            assert str(root).lower() not in output.lower(), "Private temporary path in captured output"
            vector = svg("codex-vault " + " ".join(command), output)
            (args.output / f"{name}.svg").write_text(vector, encoding="utf-8")
            if args.png:
                png(vector, args.output / f"{name}.png")
    print("Captured actual CLI output in two synthetic SVG previews.")


if __name__ == "__main__":
    main()

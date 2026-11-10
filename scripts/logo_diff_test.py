#!/usr/bin/env python3
"""Differential test of the LOGO engines on the real machine (milestone L5).

Usage: logo_diff_test.py KERNEL_ELF

For every golden script in `logo/tests/golden/*.logo` this types the script into
the real kernel (QEMU, choosing LOGO at the menu) twice: once with the default
engine, which compiles every line to RV64 machine code before running it, and
once after `.engine "ast`, the reference interpreter. Then:

  * both console transcripts must equal the hand-derived `.out` file (with the
    echo of the typed lines, which the host harness does not show, put back);
  * for the drawing scripts (those with a `;canvas` header) a screenshot taken
    through the QEMU monitor must be identical under both engines, and must show
    something drawn.

The scripts that depend on the host harness (`<BREAK AFTER n>`) are skipped.
Each run is its own QEMU process; runs go in parallel.
"""
import concurrent.futures
import os
import pathlib
import socket
import sys
import tempfile
import time

from boot_test import boot

GOLDEN = pathlib.Path(__file__).resolve().parent.parent / "logo" / "tests" / "golden"


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def monitor_command(port: int, command: str) -> None:
    with socket.create_connection(("127.0.0.1", port), timeout=5) as m:
        m.settimeout(5)
        buffer = b""
        while b"(qemu)" not in buffer:
            buffer += m.recv(4096)
        m.sendall(command.encode() + b"\n")
        time.sleep(1.0)


ROOT = GOLDEN.parent.parent.parent


def script_lines(name: str) -> list:
    """The script's lines with `<LOAD path>` replaced by that file's lines."""
    lines = []
    for line in (GOLDEN / f"{name}.logo").read_text().splitlines():
        if line.startswith("<LOAD ") and line.endswith(">"):
            lines += (ROOT / line[6:-1]).read_text().splitlines()
        else:
            lines.append(line)
    return lines


def split_prompts(typed: list, expected: str) -> list:
    """Cut the hand-derived transcript into (prompt, output) per typed line."""
    prompts, in_definition = [], False
    for line in typed:
        prompts.append("> " if in_definition else "? ")
        if in_definition and line.strip().lower() == "end":
            in_definition = False
        elif not in_definition and line.strip().lower().startswith("to "):
            in_definition = True
    parts, position = [], 0
    for i, prompt in enumerate(prompts):
        assert expected.startswith(prompt, position), (i, prompt, expected[position : position + 20])
        position += len(prompt)
        following = prompts[i + 1] if i + 1 < len(prompts) else "? "
        end = expected.find(following, position) if i + 1 < len(prompts) else len(expected) - 2
        parts.append((prompt, expected[position:end]))
        position = end
    assert expected[position:] == "? ", "transcript should end with a prompt"
    return parts


def with_echo(typed: list, expected: str, engine_lines: list) -> str:
    """The transcript as the real console shows it: each typed line echoed."""
    all_lines = engine_lines + typed
    extra = [("? ", "") for _ in engine_lines]
    parts = extra + split_prompts(typed, expected)
    out = "".join(f"{prompt}{line}\n{output}" for (prompt, output), line in zip(parts, all_lines))
    return out + "? "


def run(kernel: str, name: str, engine: str, drawing: bool) -> tuple:
    script = script_lines(name)
    typed = script[1:] if drawing else script  # the `;canvas` comment is only for the host harness
    engine_lines = [] if engine == "jit" else ['.engine "ast']
    expected = with_echo(typed, (GOLDEN / f"{name}.out").read_text(), engine_lines)
    port = free_port()
    shot = os.path.join(tempfile.mkdtemp(), "screen.ppm")
    state = {"grabbed": not drawing}

    def done(text: str) -> bool:
        if "RISCV LOGO V0.2\n\n" not in text:
            return False
        got = text.split("RISCV LOGO V0.2\n\n", 1)[1]
        if not got.startswith(expected) and expected not in got:
            return False
        if drawing and not state["grabbed"]:
            time.sleep(0.5)
            monitor_command(port, f"screendump {shot}")
            state["grabbed"] = True
        return True

    lines = [l.encode() + b"\r" for l in engine_lines + typed]
    text, timed_out, returncode = boot(
        kernel,
        [(b"".join(lines), 0.0), (done, 0.0)],
        menu_choice=b"2",
        timeout=120,
        extra_args=["-device", "ramfb"],
        monitor=f"tcp:127.0.0.1:{port},server,nowait",
        stop_when=[done],
    )
    got = text.split("RISCV LOGO V0.2\n\n", 1)[1] if "RISCV LOGO V0.2\n\n" in text else text
    picture = open(shot, "rb").read() if drawing and os.path.exists(shot) else None
    return expected, got, picture


def check_script(kernel: str, name: str) -> list:
    drawing = (GOLDEN / f"{name}.logo").read_text().startswith(";canvas")
    problems, pictures = [], {}
    for engine in ("jit", "ast"):
        expected, got, picture = run(kernel, name, engine, drawing)
        if expected not in got:
            problems.append(f"{name} [{engine}]: transcript differs\n--- expected ---\n{expected}--- got ---\n{got}")
        pictures[engine] = picture
    if drawing:
        jit, ast = pictures["jit"], pictures["ast"]
        if jit is None or ast is None:
            problems.append(f"{name}: no screenshot taken")
        elif jit != ast:
            problems.append(f"{name}: the compiled and interpreted pictures differ")
        else:
            pixels = jit.split(b"\n", 3)[3]
            if not any(pixels):
                problems.append(f"{name}: nothing was drawn")
    return problems


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    kernel = sys.argv[1]
    names = []
    for path in sorted(GOLDEN.glob("*.logo")):
        if "<BREAK AFTER" in path.read_text():
            continue
        names.append(path.stem)
    failures = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        for name, problems in zip(names, pool.map(lambda n: check_script(kernel, n), names)):
            print(f"{'ok  ' if not problems else 'FAIL'} {name}")
            failures += problems
    if failures:
        print("\nlogo diff test FAILED:", *failures[:5], sep="\n", file=sys.stderr)
        return 1
    print(f"logo diff test passed ({len(names)} scripts, compiled vs interpreted, on the machine)")
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""S4d acceptance check: the console server (`consoled`), over the serial port, in QEMU.

Boots the kernel with feature `interactive`, types `console` at init's test prompt (init then
hands the serial line to `consoled` and plays the shell on its terminal socket), and compares
EXACT output with expectations derived by hand from S4-SPEC.md section 8 (not from a run):

- echo happens in the console server, line by line, before the client sees the line;
- backspace and Ctrl-U erase on the terminal (`BS SP BS` per erased character);
- the client's `\\n` becomes `\\r\\n` on the wire (the harness strips the CR of CRLF);
- Ctrl-C echoes `^C`, kills the registered job (exit code -130), and wakes the client even
  when no job runs; Ctrl-C mid-line abandons the line;
- Ctrl-D mid-line is ignored; Ctrl-D on an empty line is end of input for the client;
- output beyond the queue bound drops the oldest bytes and says so, with exact accounting;
- the session ends cleanly and the kernel's verdict lines follow.

Usage: s4d_console_test.py INTERACTIVE_KERNEL_ELF [--break-expectation]
"""
import re
import sys

sys.path.insert(0, __import__("os").path.dirname(__file__))
import s4a_serial_test as base  # noqa: E402


def fail(session, msg):
    print(f"s4d console test FAILED: {msg}")
    print(f"last lines: {[l[:80] for l in session.seen[-8:]]}")
    session.p.kill()
    sys.exit(1)


base.fail = fail  # Session methods report through the module's `fail`


def main():
    s = base.Session(sys.argv[1])
    banner = base.BANNER
    s.skip_to(banner, "the banner")
    # init hands over the serial line; the first thing the client says
    s.step(b"console\n", [b"console ready"], "starting the console server")
    # echo, then the client's answer
    s.step(b"hello world\n", [b"hello world", b"got: hello world"], "echo and delivery")
    # BS, DEL and Ctrl-U erase with BS SP BS
    s.step(b"abx\x08c\n", [b"abx\x08 \x08c", b"got: abc"], "backspace")
    s.step(b"abx\x7fc\n", [b"abx\x08 \x08c", b"got: abc"], "delete")
    s.step(b"junk\x15ok\n", [b"junk" + b"\x08 \x08" * 4 + b"ok", b"got: ok"], "Ctrl-U")
    # an empty line is still a line
    s.step(b"\n", [b"", b"got: "], "an empty line")
    # arrow keys (escape sequences) are swallowed
    s.step(b"a\x1b[Ab\n", [b"ab", b"got: ab"], "escape sequences")
    # Ctrl-D in the middle of a line does nothing
    s.step(b"ab\x04c\n", [b"abc", b"got: abc"], "Ctrl-D mid-line")
    # Ctrl-C with no job: ^C, and the client is woken
    s.step(b"\x03", [b"^C", b"interrupt"], "Ctrl-C at an empty prompt")
    # Ctrl-C in the middle of a line abandons it
    s.step(b"partial\x03", [b"partial^C", b"interrupt"], "Ctrl-C mid-line")
    s.step(b"after\n", [b"after", b"got: after"], "the editor is clean after Ctrl-C")
    # a foreground job: registered, then killed by Ctrl-C with exit code -130
    s.step(b"spin\n", [b"spin", b"spin: started"], "starting and registering a job")
    s.step(b"\x03", [b"^C", b"interrupt", b"job: exit -130"], "Ctrl-C kills the job")
    # the job is gone and the console still works
    s.step(b"again\n", [b"again", b"got: again"], "the console after a kill")
    # a second job, killed the same way (the registration slot is reusable)
    s.step(b"spin\n", [b"spin", b"spin: started"], "a second job")
    s.step(b"\x03", [b"^C", b"interrupt", b"job: exit -130"], "Ctrl-C kills the second job")
    # output flood: far more than the queue, the socket and the serial line's pace can take (the
    # bridge moves 256 bytes a tick, so some bytes are dropped on any host speed); every byte is either shown or counted as dropped
    s.p.stdin.write(b"flood\n")
    s.p.stdin.flush()
    got = s.next_line("the echo of flood")
    if got != b"flood":
        fail(s, f"flood echo: {got!r}")
    lines = []
    while True:
        line = s.next_line("the end of the flood")
        if line == b"flood: end":
            break
        lines.append(line)
    stream = b"\r\n".join(lines) + b"\r\n"  # everything before the "flood: end" line
    if stream.count(b"flood: done\r\n") != 1:
        fail(s, "the flood's own marker is missing or repeated (the newest bytes must survive)")
    stream = stream.replace(b"flood: done\r\n", b"")
    notice = re.compile(rb"\r\n\[console: (\d+) bytes dropped\]\r\n")
    dropped = sum(int(m.group(1)) for m in notice.finditer(stream))
    rest = notice.sub(b"", stream)
    if dropped == 0:
        fail(s, "nothing was dropped by a 500000-byte flood (queue bound not enforced?)")
    if set(rest) - set(b"f\r\n"):
        fail(s, f"unexpected bytes in the flood output: {sorted(set(rest) - set(b'f' + bytes([13, 10])))}")
    # the client wrote 500000 'f' and one '\n' (-> CR LF); every byte is shown or dropped
    if len(rest) + dropped != 500000 + 2:
        fail(s, f"accounting: shown {len(rest)} + dropped {dropped} != 500002")
    # UV4: terminal modes, size and events. Expectations are derived from the spec (USER-VIEW-SPEC section 9):
    # raw delivers every byte as it arrives with no echo (so editing keys and escape sequences are data, Ctrl-C too)
    s.step(b"raw\n", [b"raw", b"mode: raw"], "entering raw mode")
    s.step(b"a\x7f\x1b[A\x04\x03\x15", [b"raw: 61", b"raw: 7f", b"raw: 1b", b"raw: 5b", b"raw: 41",
                                          b"raw: 04", b"raw: 03", b"raw: 15"], "raw bytes, unechoed")
    s.step(b"q", [b"size: 80x24", b"mode: 0", b"mode: cooked"], "leaving raw mode, default size")
    s.step(b"back\n", [b"back", b"got: back"], "cooked behaviour is back")
    # cbreak echoes; Ctrl-C is an event (no byte), not a delivered 0x03
    s.step(b"cbreak\n", [b"cbreak", b"mode: cbreak"], "entering cbreak mode")
    s.step(b"\x03", [b"^C", b"event: interrupt"], "an interrupt event")
    s.step(b"a\r", [b"a", b"raw: 61", b"size: 80x24", b"mode: 6", b"mode: cooked"], "cbreak echo and bytes")
    # the window size: set it, read it back
    s.step(b"resize\n", [b"resize", b"size: 100x30"], "setting the window size")
    # the host terminal is asked (the probe is on the wire), and its report is an event, never input
    probe = b"\x1b7\x1b[999;999H\x1b[6n\x1b8"
    s.step(b"size\n", [b"size", probe + b"size: asked"], "the size query")
    s.step(b"\x1b[40;132R", [b"event: resize 132x40"], "the size report")
    s.step(b"q", [b"size: 132x40", b"mode: 0", b"mode: cooked"], "the size is kept")
    # a report that never completes is given back as input after the query times out
    s.step(b"size\n", [b"size", probe + b"size: asked"], "a second size query")
    s.step(b"\x1b[", [b"raw: 1b", b"raw: 5b"], "a partial report is input after the timeout")
    s.step(b"q", [b"size: 132x40", b"mode: 0", b"mode: cooked"], "the size is unchanged")
    s.step(b"end\n", [b"end", b"got: end"], "cooked again")
    # end of input
    # (Ctrl-D after "x" is ignored; erasing the x leaves an empty line, where Ctrl-D ends input)
    s.step(b"x\x04\x7f\x04", [b"x\x08 \x08eof"], "Ctrl-D on an empty line is end of input")
    for want in [b"##TEST interactive_init_exits_cleanly PASS",
                 b"##TEST interactive_no_input_was_dropped PASS",
                 b"##TEST interactive_nothing_survives_init PASS",
                 b"##TEST interactive_no_kernel_thread_survives PASS"]:
        got = s.next_line("the verdict lines")
        if got != want:
            fail(s, f"verdict: expected {want!r}, got {got!r}")
    done = s.next_line("##DONE")
    if not done.startswith(b"##DONE ") or not done.endswith(b" fail=0"):
        fail(s, f"no clean ##DONE line: {done!r}")
    try:
        rc = s.p.wait(timeout=30)
    except Exception:
        fail(s, "QEMU did not exit")
    if rc != 0:
        fail(s, f"QEMU exit status {rc}")
    print("s4d console test passed")


if __name__ == "__main__":
    if "--break-expectation" in sys.argv:
        # wrap step() so one expectation is wrong: the harness must be able to fail
        orig = base.Session.step

        def step(self, send, want, what):
            if what == "backspace":
                want = [want[0], b"got: abd"]
            return orig(self, send, want, what)

        base.Session.step = step
    main()

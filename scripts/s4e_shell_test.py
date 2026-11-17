#!/usr/bin/env python3
"""S4e acceptance check: the shell session (kernel feature `interactive`: init supervises
memfsd, launcher, consoled and sh), driven over the serial port like a user.

Expectations are derived by hand from S4-SPEC.md section 8 (not from a run). The console echoes
what is typed after the prompt, so a command shows up as `$ <command>` on one line; a prompt
with nothing typed yet is unterminated and joins the next thing printed (`$ ^C`).

Usage: s4e_shell_test.py INTERACTIVE_KERNEL_ELF [--break-expectation]
"""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
import s4a_serial_test as base  # noqa: E402


def fail(session, msg):
    print(f"s4e shell test FAILED: {msg}")
    print(f"last lines: {[l[:80] for l in session.seen[-8:]]}")
    print(f"partial line: {getattr(session, 'partial', None)!r}")
    session.p.kill()
    sys.exit(1)


base.fail = fail


class Session(base.Session):
    """A session that can tell when the shell is waiting at its prompt. The prompt is an
    unterminated line (`$ `), so the line reader cannot see it: keep the partial line visible and
    wait for it before typing. (Typing early is legal, but then the echo of the first characters
    can come before the prompt, and the expected lines would depend on timing.)"""

    def __init__(self, kernel):
        self.partial = b""
        self.wait_for_prompt = True
        super().__init__(kernel)

    def _pump(self):
        buf = b""
        while True:
            c = self.p.stdout.read(1)
            if not c:
                break
            if c == b"\r":
                continue
            if c == b"\n":
                self.lines.put(buf)
                buf = b""
            else:
                buf += c
            self.partial = buf
        if buf:
            self.lines.put(buf)
        self.lines.put(None)

    def prompt(self):
        deadline = time.time() + base.TIMEOUT
        while self.partial not in (b"$ ", b"> "):
            if time.time() > deadline:
                fail(self, "timed out waiting for the prompt")
            time.sleep(0.002)

    def step(self, send, want, what):
        if self.wait_for_prompt:
            self.prompt()
        return super().step(send, want, what)

BANNER = b"riscv-os S4 ready"
BIN = [b"authd", b"basic", b"blkd", b"bomb", b"cat", b"clockd", b"consoled", b"crash", b"ctest", b"devd", b"devmgr", b"dhcpc", b"echo", b"fetch", b"fsd", b"hello", b"hog", b"httpd", b"init", b"ipchog", b"launcher", b"ls", b"lua", b"memfsd", b"netd", b"netdrv", b"netinfo", b"netsup", b"ping", b"probe", b"registryd", b"rngd", b"sh", b"spin", b"udpecho", b"volmgr"]


UUID = "6673642d73756974652d766f6c756d65"  # the volume etc/mounts calls `store`


def make_volume():
    """A doofs volume made by the host tool, attached as a virtio disk: the volume manager finds it, the table names it
    `store`, and /etc/trusted grants it to ls, cat and ctest at /mnt/store (FS-SPEC 5e)."""
    import subprocess
    import tempfile
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    if subprocess.run(["cargo", "build", "-q", "-p", "doofs-tools"], cwd=here).returncode != 0:
        print("s4e shell test FAILED: could not build the host tools")
        sys.exit(1)
    img = tempfile.NamedTemporaryFile(prefix="s4e-vol-", suffix=".img", delete=False).name
    r = subprocess.run([os.path.join(here, "target", "debug", "mkfs-doofs"), "--uuid", UUID, "--label", "shelltest",
                        "--size-mib", "4", img], capture_output=True, text=True)
    if r.returncode != 0:
        print(f"s4e shell test FAILED: mkfs-doofs: {r.stderr}")
        sys.exit(1)
    return here, img


def main():
    here, img = make_volume()
    Session.EXTRA_ARGS = ["-global", "virtio-mmio.force-legacy=false", "-drive", f"file={img},if=none,format=raw,id=hd0",
                          "-device", "virtio-blk-device,drive=hd0"]
    s = Session(sys.argv[1])
    s.skip_to(BANNER, "the banner")
    step = s.step

    # builtins
    step(b"help\n", [b"$ help", b"builtins: cd pwd exit help halt",
                     b"programs: ls cat echo probe spin hog bomb ipchog"], "help")
    step(b"pwd\n", [b"$ pwd", b"/home"], "pwd at start")
    step(b"\n", [b"$ "], "an empty line")
    step(b"# only a comment\n", [b"$ # only a comment"], "a comment")

    # programs on the fixtures
    step(b"ls\n", [b"$ ls", b"hello.txt", b"lines.txt", b"sub/"], "ls of the working directory")
    step(b"ls sub\n", [b"$ ls sub", b"inner.txt"], "ls of a subdirectory")
    step(b"ls /\n", [b"$ ls /", b"bin/", b"dev/", b"etc/", b"home/", b"include/", b"lib/", b"mnt/", b"share/", b"svc/", b"tmp/", b"usr/"], "ls of the root (trusted)")
    step(b"ls /bin\n", [b"$ ls /bin"] + BIN, "ls of /bin: sorted bytewise")
    step(b"cat hello.txt\n", [b"$ cat hello.txt", b"hello, world"], "cat")
    step(b"cat lines.txt sub/inner.txt\n",
         [b"$ cat lines.txt sub/inner.txt", b"one", b"two", b"three", b"inner"], "cat of two files")
    step(b"cat /etc/trusted\n", [b"$ cat /etc/trusted", b"ls store", b"cat store", b"ctest store"], "the trust list")
    step(b"echo a   'b  c' \"d\"\n", [b"$ echo a   'b  c' \"d\"", b"a b  c d"], "echo and quoting")
    step(b"echo\n", [b"$ echo", b""], "echo with no arguments")

    # errors of programs, with their exit codes
    step(b"cat nope\n", [b"$ cat nope", b"cat: nope: not found", b"[exit 1]"], "cat: missing file")
    step(b"cat sub\n", [b"$ cat sub", b"cat: sub: not a file", b"[exit 1]"], "cat: a directory")
    step(b"cat ../x\n", [b"$ cat ../x", b"cat: ../x: bad path", b"[exit 1]"], "cat: a path with ..")
    step(b"ls nope\n", [b"$ ls nope", b"ls: nope: not found", b"[exit 1]"], "ls: missing directory")
    step(b"ls hello.txt\n", [b"$ ls hello.txt", b"ls: hello.txt: not a directory", b"[exit 1]"],
         "ls: a file")

    # pipelines
    step(b"echo hi | cat\n", [b"$ echo hi | cat", b"hi"], "a two-stage pipeline")
    step(b"echo one|cat|cat\n", [b"$ echo one|cat|cat", b"one"], "a three-stage pipeline")
    step(b"ls sub | cat | cat\n", [b"$ ls sub | cat | cat", b"inner.txt"], "ls into cat")
    step(b"cat nope | cat\n", [b"$ cat nope | cat", b"cat: nope: not found", b"[exit 1]"],
         "a failing first stage is reported")

    # cd and pwd are lexical
    step(b"cd sub\n", [b"$ cd sub"], "cd")
    step(b"pwd\n", [b"$ pwd", b"/home/sub"], "pwd after cd")
    step(b"ls\n", [b"$ ls", b"inner.txt"], "ls in the new directory")
    step(b"cat inner.txt\n", [b"$ cat inner.txt", b"inner"], "cat in the new directory")
    step(b"cd ../..\n", [b"$ cd ../.."], "cd to the root")
    step(b"pwd\n", [b"$ pwd", b"/"], "pwd at the root")
    step(b"cd ..\n", [b"$ cd .."], "cd above the root stays at the root (the chroot rule)")
    step(b"pwd\n", [b"$ pwd", b"/"], "pwd is still /")
    step(b"cd nope\n", [b"$ cd nope", b"cd: nope: not found"], "cd to a missing directory")
    step(b"cd home/hello.txt\n", [b"$ cd home/hello.txt", b"cd: home/hello.txt: not a directory"],
         "cd to a file")
    step(b"cd /home/sub\n", [b"$ cd /home/sub"], "cd with an absolute path")
    step(b"pwd\n", [b"$ pwd", b"/home/sub"], "pwd after an absolute cd")
    step(b"cd\n", [b"$ cd", b"cd: usage: cd <dir>"], "cd without an argument")
    step(b"cd /home\n", [b"$ cd /home"], "back to /home")

    # shell errors
    step(b"frob\n", [b"$ frob", b"sh: frob: not found"], "an unknown command")
    step(b"bin/ls\n", [b"$ bin/ls", b"sh: bin/ls: bad command name"], "a command name with a slash")
    step(b"echo 'x\n", [b"$ echo 'x", b"sh: unterminated quote"], "an unterminated quote")
    step(b"| ls\n", [b"$ | ls", b"sh: empty command in pipeline"], "an empty stage")
    step(b"ls | cd sub\n", [b"$ ls | cd sub", b"sh: cd: a builtin cannot be piped"], "a piped builtin")
    step(b"echo ok | frob\n", [b"$ echo ok | frob", b"sh: frob: not found"], "an unknown stage")
    step(b"echo still alive\n", [b"$ echo still alive", b"still alive"], "the shell after errors")

    # volumes (FS-SPEC 5e): the volume manager serves the host-made disk, the mount table names it `store`, and /etc/trusted
    # grants it to ls, cat and ctest at /mnt/store. The volume comes up a moment after the prompt.
    time.sleep(4)
    step(b"ls /mnt/store\n", [b"$ ls /mnt/store"], "the new volume is empty")
    step(b"ctest mount /mnt/store\n",
         [b"$ ctest mount /mnt/store",
          b"ok: create a file in the volume", b"ok: write it", b"ok: close it", b"ok: open it again",
          b"ok: read the same bytes", b"ok: stat reports its size", b"ok: rename inside the volume",
          b"ok: the old name is gone", b"ok: unlink it", b"ok: make a directory", b"ok: remove it",
          b"ok: a path with .. does not leave the volume",
          b"ok: getcwd returns an absolute path", b"ok: chdir into the volume", b"ok: getcwd names it",
          b"ok: chdir with .. climbs out of the volume to the root's /home", b"ok: and getcwd says so",
          b"ok: open a directory by path", b"ok: openat with .. from a path-annotated directory descriptor",
          b"ok: `..` above the root stays at the root", b"ok: the empty path names nothing",
          b"ok: write to /dev/null", b"ok: read zeros from /dev/zero", b"ok: write a file in the private /tmp",
          b"ok: a new file has the default mode 0644", b"ok: one link, its size and a modification time", b"ok: chmod, then stat shows the mode", b"ok: access sees the execute bit", b"ok: and sees it cleared", b"ok: chown sets the advisory owner and group", b"ok: fchmod and fstat on a descriptor", b"ok: mkdir applies the umask", b"ok: make a symlink", b"ok: lstat describes the link itself", b"ok: stat follows it", b"ok: readlink gives the target text", b"ok: an absolute link target is refused", b"ok: hard links are refused",
          b"ok: leave a file for the next session",
          b"mount: 0 failures"],
         "a C program makes, reads, renames and removes files in the granted volume")
    step(b"ls /mnt/store\n", [b"$ ls /mnt/store", b"kept"], "what it left behind")
    step(b"ls -l /mnt/store\n", [b"$ ls -l /mnt/store", b"-rw-r--r-- 21 kept"], "ls -l shows the advisory mode and the size")
    step(b"ls -l /home\n", [b"$ ls -l /home", b"-rw-r--r-- 13 hello.txt", b"-rw-r--r-- 14 lines.txt", b"drwxr-xr-x 1 sub/"],
         "ls -l on the boot image: files 0644, directories 0755")
    step(b"ls -l /usr\n", [b"$ ls -l /usr", b"lrwxrwxrwx 6 bin -> ../bin", b"lrwxrwxrwx 10 include -> ../include",
                           b"lrwxrwxrwx 6 lib -> ../lib", b"lrwxrwxrwx 8 share -> ../share"],
         "ls -l shows links and their targets")
    step(b"ls /tmp\n", [b"$ ls /tmp"], "the program's private /tmp is gone with it")
    step(b"cat /mnt/store/kept\n", [b"$ cat /mnt/store/kept", b"kept across sessions"], "cat reads inside the volume")
    step(b"echo hi | cat /mnt/store/kept\n", [b"$ echo hi | cat /mnt/store/kept", b"kept across sessions"],
         "a pipeline with a volume path")
    # a program that is not named in /etc/trusted gets no volume, and nothing named by the table leaks to it
    step(b"probe\n", [b"$ probe", b"open /etc/trusted: -108", b"open ../x: -108", b"write ./f: -3",
                      b"clone: ok", b"clone all: -3", b"root: absent", b"dev null: 0", b"dev zero: 0", b"tmp: 0", b"spawn: -5"],
         "probe has no volume")
    # SH4: `ns` lists the table the shell holds; `confine` narrows what programs started inside can reach, and only narrows
    step(b"ns\n", [b"$ ns", b'[{"path":"/","rights":"r"},{"path":".","rights":"r"},{"path":"/dev","rights":"rw"},{"path":"/tmp","rights":"rw"},{"path":"/svc","rights":"r"}]'],
         "ns lists the shell's table")
    step(b'confine { "/home": "r" } do cat /home/hello.txt end\n',
         [b'$ confine { "/home": "r" } do cat /home/hello.txt end', b"hello, world"], "confine: a granted path is readable")
    step(b'confine { "/home": "r" } do cat /etc/trusted end\n',
         [b'$ confine { "/home": "r" } do cat /etc/trusted end', b"cat: /etc/trusted: not found", b"[exit 1]"],
         "confine: a path that was not granted is not there, even for a trusted program")
    step(b'confine { "/home": "rw" } do cat /home/hello.txt end\n',
         [b'$ confine { "/home": "rw" } do cat /home/hello.txt end', b"sh: confine: permission denied"],
         "confine cannot widen: the shell's root is read-only")
    step(b'confine { "/mnt/store": "r" } do ls /mnt/store end\n',
         [b'$ confine { "/mnt/store": "r" } do ls /mnt/store end', b"kept"], "confine: a volume from the registry")
    step(b'confine { "/nope": "r" } do ls end\n',
         [b'$ confine { "/nope": "r" } do ls end', b"sh: confine: not found"], "confine: a grant that does not exist")
    step(b"ls /\n", [b"$ ls /", b"bin/", b"dev/", b"etc/", b"home/", b"include/", b"lib/", b"mnt/", b"share/", b"svc/", b"tmp/", b"usr/"],
         "the confinement ended with the block")
    # SH5: the line editor. The shell puts the terminal in raw mode and edits the line itself, so what it writes is derived
    # by hand from the editor's rules (ish/src/line.rs): a typed character is echoed, Backspace is BS SP BS, a character
    # typed in the middle reprints the rest and moves back with ESC [ 1 D, Enter moves to the end of the line first.
    step(b"echo hel\x7flo\r", [b"$ echo hel\x08 \x08lo", b"helo"], "editing: backspace")
    step(b"echo ac\x1b[Db\r", [b"$ echo ac\x1b[1Dbc\x1b[1D\x1b[1C", b"abc"], "editing: Left, then a character in the middle")
    step(b"echo hist\r", [b"$ echo hist", b"hist"], "history: a command is run")
    step(b"\x1b[A\r", [b"$ echo hist", b"hist"], "history: Up brings it back")
    step(b"cat hel\t\r", [b"$ cat hello.txt ", b"hello, world"], "completion: a file name from the shell's table")
    step(b"ls su\t\x03", [b"$ ls sub/^C"], "completion: a directory gets a slash and no space")
    step(b"ech\t hi\r", [b"$ echo  hi", b"hi"], "completion: a program name from /bin")
    step(b"if true do\r", [b"$ if true do"], "a block that is not finished")
    step(b"echo ml\r", [b"> echo ml"], "the continuation prompt")
    step(b"end\r", [b"> end", b"ml"], "the block runs when it is complete")
    step(b"echo abc\x03", [b"$ echo abc^C"], "Ctrl-C abandons the line being typed")
    step(b"echo ok\r", [b"$ echo ok", b"ok"], "the shell after an abandoned line")
    step(b"while true do end\r", [b"$ while true do end"], "a loop written in shell code")
    time.sleep(1)
    s.wait_for_prompt = False  # the shell is busy: there is no prompt to wait for
    step(b"\x03", [b"^C"], "Ctrl-C stops the loop")
    s.wait_for_prompt = True
    step(b"echo alive\r", [b"$ echo alive", b"alive"], "the shell after an interrupted loop")
    # the session ends and a fresh one starts: the volume is durable
    step(b"exit\n", [b"$ exit", b"init: autologin: user", BANNER], "a new session")
    time.sleep(4)
    step(b"cat /mnt/store/kept\n", [b"$ cat /mnt/store/kept", b"kept across sessions"], "the file in the next session")

    # BASIC (S7a): the home computer's interpreter as an ordinary program on the terminal
    step(b"basic\n", [b"$ basic", b"READY."], "basic starts and prompts")
    s.wait_for_prompt = False  # basic has no "$ " prompt: the steps below talk to BASIC
    step(b"PRINT 6*7\n", [b"PRINT 6*7", b" 42 ", b"READY."], "basic evaluates an immediate statement")
    step(b'10 PRINT "FROM A PROGRAM"\n', [b'10 PRINT "FROM A PROGRAM"', b"READY."], "basic stores a program line")
    step(b"RUN\n", [b"RUN", b"FROM A PROGRAM", b"READY."], "basic runs it")
    step(b"PRINT 2+2\n", [b"PRINT 2+2", b" 4 ", b"READY."], "basic stays alive")
    step(b"\x04", [], "Ctrl-D at an empty line ends basic")
    s.prompt()  # (exit 0 prints nothing: the shell is simply back at its prompt)
    step(b"basic\n", [b"$ basic", b"READY."], "basic again")
    s.wait_for_prompt = False
    step(b"BYE\n", [b"BYE"], "BYE ends basic")
    s.prompt()
    s.wait_for_prompt = True
    step(b"echo back at the shell\n", [b"$ echo back at the shell", b"back at the shell"],
         "the shell after basic")

    # Lua (S7d): the upstream interpreter's read-eval-print loop on the terminal
    s.wait_for_prompt = True
    step(b"lua\n", [b"$ lua", b"Lua 5.4.8  Copyright (C) 1994-2025 Lua.org, PUC-Rio"], "lua starts")
    s.wait_for_prompt = False  # lua's own prompt is "> "

    def lua_prompt():  # (typing before the prompt is printed would interleave it with the echo)
        deadline = time.time() + base.TIMEOUT
        while s.partial != b"> ":
            if time.time() > deadline:
                fail(s, "timed out waiting for lua's prompt")
            time.sleep(0.002)

    lua_prompt()
    step(b"print(6*7)\n", [b"> print(6*7)", b"42"], "lua evaluates a statement")
    lua_prompt()
    step(b"x = 5\n", [b"> x = 5"], "lua assigns")
    lua_prompt()
    step(b"x * 2\n", [b"> x * 2", b"10"], "lua prints the value of an expression")
    lua_prompt()
    step(b"for i=1,3 do io.write(i, ' ') end print()\n",
         [b"> for i=1,3 do io.write(i, ' ') end print()", b"1 2 3 "], "lua runs a loop")
    lua_prompt()
    step(b"error('oops')\n", [b"> error('oops')", b"stdin:1: oops", b"stack traceback:",
                              b"\t[C]: in function 'error'", b"\tstdin:1: in main chunk", b"\t[C]: in ?"],
         "lua reports an error and carries on")
    lua_prompt()
    step(b"print(x)\n", [b"> print(x)", b"5"], "lua kept its state")
    lua_prompt()
    step(b"\x04", [b"> "], "Ctrl-D at an empty line ends lua")
    s.prompt()
    s.wait_for_prompt = True
    step(b"echo back from lua\n", [b"$ echo back from lua", b"back from lua"], "the shell after lua")

    # confinement: `probe` has only a read-only working directory
    step(b"probe\n", [b"$ probe", b"open /etc/trusted: -108", b"open ../x: -108",
                      b"write ./f: -3", b"clone: ok", b"clone all: -3", b"root: absent",
                      b"dev null: 0", b"dev zero: 0", b"tmp: 0", b"spawn: -5"],
         "probe: nothing outside the working directory")

    # limits (S4.5): a program is the only process of its job, runs at most four threads, and
    # has a bounded amount of memory, whatever it tries
    step(b"bomb\n", [b"$ bomb", b"bomb: process_create: -5", b"bomb: threads: 31 then -5"],
         "a process and thread bomb is refused")
    counts = []
    for attempt in (1, 2):
        step(b"hog\n", [b"$ hog"], f"a memory hog, run {attempt}")
        mib = 0
        while True:
            line = s.next_line("the hog's output")
            if line == f"hog: refused at {mib} MiB".encode():
                break
            mib += 1
            if line != f"hog: {mib} MiB".encode():
                fail(s, f"hog output: expected 'hog: {mib} MiB', got {line!r}")
        line = s.next_line("the hog's exit")
        if line != b"[exit 7]":
            fail(s, f"the hog ended with {line!r}, wanted [exit 7]")
        counts.append(mib)
    # the heap stops at 128 MiB (some of it is the program's own); the 136 MiB job never has to refuse
    if not 120 <= counts[0] <= 128:
        fail(s, f"the hog got {counts[0]} MiB of a 128 MiB heap")
    if counts[0] != counts[1]:
        fail(s, f"the second hog got {counts[1]} MiB, the first {counts[0]}: memory was not given back")
    step(b"echo survived\n", [b"$ echo survived", b"survived"], "the shell after the hog")

    # quotas on kernel objects and queued data (S4.5c): the hog is stopped by its own job, twice
    # with identical results, and the pipes of the shell keep working around it
    results = []
    for attempt in (1, 2):
        step(b"ipchog\n", [b"$ ipchog", b"ipchog: queued 192 KiB"], f"an ipc and object hog, run {attempt}")
        line = s.next_line("the hog's event count")
        m = re.fullmatch(rb"ipchog: made (\d+) events", line)
        if not m:
            fail(s, f"ipchog: expected the event count, got {line!r}")
        results.append(int(m.group(1)))
        step(b"echo through | cat | cat\n", [b"$ echo through | cat | cat", b"through"],
             "pipes work right after the hog")
    if not 180 <= results[0] <= 250:
        fail(s, f"the hog made {results[0]} events of a 256-object job")
    if results[0] != results[1]:
        fail(s, f"second run {results[1]} events, first {results[0]}: objects were not given back")

    # Ctrl-C
    step(b"spin\n", [b"$ spin", b"spinning"], "a running job")
    s.wait_for_prompt = False  # a job is running: there is no prompt until it is gone
    step(b"\x03", [b"^C", b"[exit -130]"], "Ctrl-C kills the job")
    s.wait_for_prompt = True
    step(b"echo after\n", [b"$ echo after", b"after"], "one prompt after the kill (no extra prompt)")
    step(b"\x03", [b"$ ^C"], "Ctrl-C at an idle prompt")
    step(b"echo idle\n", [b"$ echo idle", b"idle"], "the shell after an idle Ctrl-C")
    # a Ctrl-C typed together with the command, before the job is even registered, still ends it
    # (the job may or may not have printed its first line yet)
    step(b"spin\n\x03", [b"$ spin"], "a command and Ctrl-C in one write")
    s.wait_for_prompt = False
    while True:
        line = s.next_line("the early Ctrl-C")
        if line == b"spinning":
            continue
        if line != b"^C":
            fail(s, f"early Ctrl-C: expected ^C, got {line!r}")
        break
    line = s.next_line("the early Ctrl-C report")
    if line == b"spinning":  # (printed between the echo and the report)
        line = s.next_line("the early Ctrl-C report")
    if line != b"[exit -130]":
        fail(s, f"early Ctrl-C: expected [exit -130], got {line!r}")
    s.wait_for_prompt = True
    step(b"echo calm\n", [b"$ echo calm", b"calm"], "one prompt after an early Ctrl-C")
    step(b"spin | cat\n", [b"$ spin | cat", b"spinning"], "a running pipeline")
    s.wait_for_prompt = False
    step(b"\x03", [b"^C", b"[exit -130]", b"[exit -130]"], "Ctrl-C kills every stage")
    s.wait_for_prompt = True
    step(b"echo again\n", [b"$ echo again", b"again"], "the shell after a pipeline kill")

    # jobs (SHELL-SPEC J1-J9, SH4b): spawn returns at once, poll sees it running, a bounded wait times out and leaves it
    # running, kill ends it, and Ctrl-C during a wait is an `interrupted` failure that try can catch and defer survives
    step(b'k = spawn { "out": "discard" } spin\n', [b'$ k = spawn { "out": "discard" } spin'], "a background job starts at once")
    step(b"print (poll $k)\n", [b"$ print (poll $k)", b"null"], "the job is still running")
    step(b'try do wait { "timeout": 300 } $k end catch e do print $e.kind end\n',
         [b'$ try do wait { "timeout": 300 } $k end catch e do print $e.kind end', b"timeout"], "a bounded wait times out")
    step(b"print (poll $k)\n", [b"$ print (poll $k)", b"null"], "the job survived the timeout")
    step(b"fn w do defer do print cleanup end; wait $k end\n", [b"$ fn w do defer do print cleanup end; wait $k end"], "a function that waits")
    step(b"try do w end catch e do print caught $e end\n", [b"$ try do w end catch e do print caught $e end"], "waiting for the job")
    time.sleep(1)
    s.wait_for_prompt = False
    step(b"\x03", [b"cleanup", b"caught interrupted"], "Ctrl-C ends the wait: defer runs, try catches")
    s.wait_for_prompt = True
    step(b"print (poll $k)\n", [b"$ print (poll $k)", b"null"], "the job is still running after the interrupt")
    step(b"w\n", [b"$ w"], "an uncaught interrupt in a wait")
    time.sleep(1)
    s.wait_for_prompt = False
    step(b"\x03", [b"cleanup", b"^C"], "Ctrl-C ends the wait: defer runs, the shell reports it")
    s.wait_for_prompt = True
    step(b"kill $k\n", [b"$ kill $k"], "kill ends the job")
    step(b"r = wait $k\n", [b"$ r = wait $k"], "the killed job's result")
    step(b"print $r.status\n", [b"$ print $r.status", b"-1"], "the killed job's status")
    step(b"echo done\n", [b"$ echo done", b"done"], "the shell after the jobs")

    # end of input and exit: the shell ends and a fresh session starts
    step(b"\x04", [b"$ init: autologin: user", BANNER], "Ctrl-D ends the shell; init starts a new one")
    step(b"pwd\n", [b"$ pwd", b"/home"], "a fresh session starts in /home")
    step(b"exit\n", [b"$ exit"], "exit")
    line = s.next_line("the autologin log after exit")
    if line != b"init: autologin: user":
        fail(s, f"after exit: expected the autologin log line, got {line!r}")
    line = s.next_line("the banner after exit")
    if line != BANNER:  # (the echoed `exit` ended the prompt's line)
        fail(s, f"after exit: expected a new banner, got {line!r}")
    step(b"echo fresh\n", [b"$ echo fresh", b"fresh"], "the new session works")

    # halt: init ends the session and the kernel checks for leaks
    step(b"halt\n", [b"$ halt"], "halt")
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
    import subprocess
    r = subprocess.run([os.path.join(here, "target", "debug", "fsck-doofs"), img], capture_output=True, text=True)
    if r.returncode != 0:
        fail(s, f"the host fsck rejects the volume: {r.stdout}{r.stderr}")
    os.unlink(img)
    print("s4e shell test passed")


if __name__ == "__main__":
    if "--break-expectation" in sys.argv:
        orig = base.Session.step

        def step(self, send, want, what):
            if what == "pwd after cd":
                want = [want[0], b"/home/sob"]
            return orig(self, send, want, what)

        base.Session.step = step
    main()

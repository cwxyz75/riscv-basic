//! A host in memory, for tests: files, an environment, pipes as buffers, and "programs" that are Rust functions run to
//! completion when started. A read from an empty pipe that still has a writer is an error (a would-be deadlock), not a hang.

use crate::host::*;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// What a fake program sees and does.
pub struct ProgCtx<'a> {
    pub args: &'a [String],
    pub input: Vec<u8>,
    pub out: Vec<u8>,
    pub err: Vec<u8>,
    pub files: &'a BTreeMap<String, Vec<u8>>,
    pub cwd: &'a str,
    /// What the program may reach: `(path, rights)`.
    pub table: &'a [(String, String)],
    /// The environment the program was started with.
    pub env: &'a [(String, String)],
    /// How long the program runs on the fake clock, in milliseconds (`u64::MAX`: until killed).
    pub run_ms: u64,
}

pub type Prog = fn(&mut ProgCtx) -> i32;

enum Obj {
    Stdin,
    Stdout,
    Stderr,
    PipeRead(u32),
    PipeWrite(u32),
    FileRead { data: Vec<u8>, pos: usize },
    FileWrite(String),
    Null,
}

#[derive(Default)]
struct Pipe {
    buf: VecDeque<u8>,
    writers: u32,
    readers: u32,
}

pub struct FakeHost {
    pub files: BTreeMap<String, Vec<u8>>,
    env: BTreeMap<String, String>,
    cwd: String,
    stdin_buf: VecDeque<u8>,
    pub stdout_buf: Vec<u8>,
    pub stderr_buf: Vec<u8>,
    objs: BTreeMap<u32, Obj>,
    pipes: BTreeMap<u32, Pipe>,
    next: u32,
    progs: BTreeMap<String, Prog>,
    /// pid -> (exit status, the fake time in milliseconds at which the program ends)
    statuses: BTreeMap<u32, (i32, u64)>,
    /// The fake clock, advanced only by waiting.
    pub now_ms: u64,
    /// A wait with a timeout reports an interrupt (to test J9).
    pub interrupt_in_wait: bool,
    /// Every program started, in order, with its arguments.
    pub log: Vec<Vec<String>>,
    confined: Vec<Vec<(String, String)>>,
    /// What the terminal will give, in order (then it is gone).
    pub term_in: VecDeque<TermIn>,
    /// `interrupted()` answers true on this many polls from now (0 = never).
    pub interrupt_after_polls: u32,
}

fn norm(cwd: &str, p: &str) -> String {
    let full = if p.starts_with('/') {
        String::from(p)
    } else {
        alloc::format!("{}/{}", cwd.trim_end_matches('/'), p)
    };
    let mut parts: Vec<&str> = Vec::new();
    for c in full.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    alloc::format!("/{}", parts.join("/"))
}

impl FakeHost {
    pub fn new() -> FakeHost {
        let mut h = FakeHost {
            files: BTreeMap::new(),
            env: BTreeMap::new(),
            cwd: String::from("/"),
            stdin_buf: VecDeque::new(),
            stdout_buf: Vec::new(),
            stderr_buf: Vec::new(),
            objs: BTreeMap::new(),
            pipes: BTreeMap::new(),
            next: 10,
            progs: BTreeMap::new(),
            statuses: BTreeMap::new(),
            now_ms: 0,
            interrupt_in_wait: false,
            log: Vec::new(),
            confined: Vec::new(),
            term_in: VecDeque::new(),
            interrupt_after_polls: 0,
        };
        h.objs.insert(0, Obj::Stdin);
        h.objs.insert(1, Obj::Stdout);
        h.objs.insert(2, Obj::Stderr);
        for (n, p) in standard_programs() {
            h.progs.insert(String::from(n), p);
        }
        h
    }

    pub fn add_prog(&mut self, name: &str, p: Prog) {
        self.progs.insert(String::from(name), p);
    }

    pub fn with_file(mut self, path: &str, content: &str) -> FakeHost {
        self.files
            .insert(norm("/", path), content.as_bytes().to_vec());
        self
    }

    /// A file with arbitrary bytes (a log that is not valid UTF-8).
    pub fn with_file_bytes(mut self, path: &str, content: &[u8]) -> FakeHost {
        self.files.insert(norm("/", path), content.to_vec());
        self
    }

    pub fn set_env(&mut self, name: &str, value: &str) {
        self.env.insert(String::from(name), String::from(value));
    }

    pub fn set_stdin(&mut self, bytes: &[u8]) {
        self.stdin_buf = bytes.iter().copied().collect();
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout_buf).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr_buf).into_owned()
    }

    pub fn file_text(&self, path: &str) -> Option<String> {
        self.files
            .get(&norm(&self.cwd, path))
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    /// Streams still open (a leak check: the shell must close what it opens).
    pub fn open_streams(&self) -> usize {
        self.objs.len() - 3
    }

    fn alloc(&mut self, o: Obj) -> Stream {
        let id = self.next;
        self.next += 1;
        match &o {
            Obj::PipeRead(p) => self.pipes.get_mut(p).expect("pipe").readers += 1,
            Obj::PipeWrite(p) => self.pipes.get_mut(p).expect("pipe").writers += 1,
            _ => {}
        }
        self.objs.insert(id, o);
        Stream(id)
    }

    fn write_obj(&mut self, id: u32, bytes: &[u8]) -> Result<(), HostError> {
        match self.objs.get(&id) {
            Some(Obj::Stdout) => self.stdout_buf.extend_from_slice(bytes),
            Some(Obj::Stderr) => self.stderr_buf.extend_from_slice(bytes),
            Some(Obj::Null) => {}
            Some(Obj::PipeWrite(p)) => {
                let p = *p;
                let pipe = self.pipes.get_mut(&p).expect("pipe");
                if pipe.readers == 0 {
                    return Err(HostError::Closed);
                }
                pipe.buf.extend(bytes.iter().copied());
            }
            Some(Obj::FileWrite(path)) => {
                let path = path.clone();
                self.files.entry(path).or_default().extend_from_slice(bytes);
            }
            _ => return Err(HostError::Other(String::from("not writable"))),
        }
        Ok(())
    }

    fn read_obj(&mut self, id: u32, buf: &mut [u8]) -> Result<Option<usize>, HostError> {
        // Ok(None): would block
        match self.objs.get_mut(&id) {
            Some(Obj::Stdin) => {
                let n = buf.len().min(self.stdin_buf.len());
                for b in buf.iter_mut().take(n) {
                    *b = self.stdin_buf.pop_front().unwrap_or(0);
                }
                Ok(Some(n))
            }
            Some(Obj::FileRead { data, pos }) => {
                let n = buf.len().min(data.len() - *pos);
                buf[..n].copy_from_slice(&data[*pos..*pos + n]);
                *pos += n;
                Ok(Some(n))
            }
            Some(Obj::Null) => Ok(Some(0)),
            Some(Obj::PipeRead(p)) => {
                let p = *p;
                let pipe = self.pipes.get_mut(&p).expect("pipe");
                if pipe.buf.is_empty() {
                    return Ok(if pipe.writers == 0 { Some(0) } else { None });
                }
                let n = buf.len().min(pipe.buf.len());
                for b in buf.iter_mut().take(n) {
                    *b = pipe.buf.pop_front().unwrap_or(0);
                }
                Ok(Some(n))
            }
            _ => Err(HostError::Other(String::from("not readable"))),
        }
    }

    fn read_all(&mut self, id: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunk = [0u8; 512];
        while let Ok(Some(n)) = self.read_obj(id, &mut chunk) {
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
        }
        out
    }
}

impl Default for FakeHost {
    fn default() -> FakeHost {
        FakeHost::new()
    }
}

impl Host for FakeHost {
    fn stdin(&self) -> Stream {
        Stream(0)
    }
    fn stdout(&self) -> Stream {
        Stream(1)
    }
    fn stderr(&self) -> Stream {
        Stream(2)
    }

    fn spawn(
        &mut self,
        argv: &[String],
        env: &[(String, String)],
        io: ProcIo,
    ) -> Result<Pid, HostError> {
        let name = argv[0].rsplit('/').next().unwrap_or("");
        let Some(prog) = self.progs.get(name).copied() else {
            return Err(HostError::NotFound);
        };
        self.log.push(argv.to_vec());
        let input = self.read_all(io.stdin.0);
        let files = self.files.clone();
        let cwd = self.cwd.clone();
        let table = self.table();
        let mut ctx = ProgCtx {
            table: &table,
            env,
            run_ms: 0,
            args: argv,
            input,
            out: Vec::new(),
            err: Vec::new(),
            files: &files,
            cwd: &cwd,
        };
        let status = prog(&mut ctx);
        let (out, err) = (core::mem::take(&mut ctx.out), core::mem::take(&mut ctx.err));
        self.write_obj(io.stdout.0, &out).ok();
        self.write_obj(io.stderr.0, &err).ok();
        let pid = self.next;
        self.next += 1;
        let ends = if ctx.run_ms == u64::MAX {
            u64::MAX
        } else {
            self.now_ms + ctx.run_ms
        };
        self.statuses.insert(pid, (status, ends));
        Ok(Pid(pid))
    }

    fn wait(&mut self, pid: Pid) -> i32 {
        match self.statuses.remove(&pid.0) {
            Some((st, ends)) => {
                if ends != u64::MAX {
                    self.now_ms = self.now_ms.max(ends);
                }
                st
            }
            None => -1,
        }
    }

    fn try_wait(&mut self, pid: Pid) -> Option<i32> {
        match self.statuses.get(&pid.0) {
            Some(&(st, ends)) if ends <= self.now_ms => {
                self.statuses.remove(&pid.0);
                Some(st)
            }
            Some(_) => None,
            None => Some(-1),
        }
    }

    fn wait_any(&mut self, pids: &[Pid]) -> (usize, i32) {
        let mut best: Option<(usize, u64)> = None;
        for (i, p) in pids.iter().enumerate() {
            let ends = self.statuses.get(&p.0).map_or(0, |&(_, e)| e);
            if best.is_none_or(|(_, b)| ends < b) {
                best = Some((i, ends));
            }
        }
        let (i, _) = best.unwrap_or((0, 0));
        let st = self.wait(pids[i]);
        (i, st)
    }

    fn wait_timeout(&mut self, pid: Pid, ms: u64) -> WaitOutcome {
        if self.interrupt_in_wait {
            return WaitOutcome::Interrupted;
        }
        match self.statuses.get(&pid.0) {
            None => WaitOutcome::Done(-1),
            Some(&(_, ends)) if ends != u64::MAX && ends <= self.now_ms + ms => {
                WaitOutcome::Done(self.wait(pid))
            }
            Some(_) => {
                self.now_ms += ms;
                WaitOutcome::Timeout
            }
        }
    }

    fn pipe(&mut self) -> Result<(Stream, Stream), HostError> {
        let id = self.next;
        self.next += 1;
        self.pipes.insert(id, Pipe::default());
        let r = self.alloc(Obj::PipeRead(id));
        let w = self.alloc(Obj::PipeWrite(id));
        Ok((r, w))
    }

    fn read(&mut self, s: Stream, buf: &mut [u8]) -> Result<usize, HostError> {
        match self.read_obj(s.0, buf)? {
            Some(n) => Ok(n),
            None => Err(HostError::Other(String::from(
                "read would block forever (a writer is still open)",
            ))),
        }
    }

    fn poll_read(
        &mut self,
        streams: &[Stream],
        buf: &mut [u8],
    ) -> Result<(usize, usize), HostError> {
        for (i, s) in streams.iter().enumerate() {
            if let Some(n) = self.read_obj(s.0, buf)? {
                return Ok((i, n));
            }
        }
        Err(HostError::Other(String::from(
            "poll would block forever (a writer is still open)",
        )))
    }

    fn write(&mut self, s: Stream, bytes: &[u8]) -> Result<(), HostError> {
        self.write_obj(s.0, bytes)
    }

    fn close(&mut self, s: Stream) {
        if s.0 < 3 {
            return;
        }
        match self.objs.remove(&s.0) {
            Some(Obj::PipeRead(p)) => {
                if let Some(pipe) = self.pipes.get_mut(&p) {
                    pipe.readers -= 1;
                }
            }
            Some(Obj::PipeWrite(p)) => {
                if let Some(pipe) = self.pipes.get_mut(&p) {
                    pipe.writers -= 1;
                }
            }
            _ => {}
        }
    }

    fn open(&mut self, path: &str, mode: OpenMode) -> Result<Stream, HostError> {
        let full = norm(&self.cwd, path);
        if full == "/dev/null" {
            return Ok(self.alloc(Obj::Null));
        }
        match mode {
            OpenMode::Read => {
                let data = self.files.get(&full).cloned().ok_or(HostError::NotFound)?;
                Ok(self.alloc(Obj::FileRead { data, pos: 0 }))
            }
            OpenMode::Write => {
                self.files.insert(full.clone(), Vec::new());
                Ok(self.alloc(Obj::FileWrite(full)))
            }
            OpenMode::Append => {
                self.files.entry(full.clone()).or_default();
                Ok(self.alloc(Obj::FileWrite(full)))
            }
        }
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, HostError> {
        self.files
            .get(&norm(&self.cwd, path))
            .cloned()
            .ok_or(HostError::NotFound)
    }

    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), HostError> {
        self.files.insert(norm(&self.cwd, path), bytes.to_vec());
        Ok(())
    }

    fn glob(&mut self, pattern: &str) -> Vec<String> {
        let abs = pattern.starts_with('/');
        let full = norm(&self.cwd, pattern);
        let want: Vec<&str> = full.trim_start_matches('/').split('/').collect();
        let mut out = Vec::new();
        for path in self.files.keys() {
            let have: Vec<&str> = path.trim_start_matches('/').split('/').collect();
            if have.len() == want.len()
                && have
                    .iter()
                    .zip(&want)
                    .all(|(h, w)| crate::builtins::glob_match(w, h))
            {
                out.push(if abs {
                    path.clone()
                } else {
                    path.strip_prefix(&alloc::format!("{}/", self.cwd.trim_end_matches('/')))
                        .unwrap_or(path)
                        .to_string()
                });
            }
        }
        out.sort();
        out
    }

    fn table(&self) -> Vec<(String, String)> {
        match self.confined.last() {
            Some(t) => t.clone(),
            None => [("/", "rw"), (".", "r"), ("/dev", "rw"), ("/tmp", "rw")]
                .iter()
                .map(|(p, r)| (String::from(*p), String::from(*r)))
                .collect(),
        }
    }

    fn confine(&mut self, grants: &[(String, String)]) -> Result<(), HostError> {
        let now = self.table();
        for (path, rights) in grants {
            let within = |entry: &str| {
                entry == "/"
                    || path == entry
                    || path.strip_prefix(entry).is_some_and(|r| r.starts_with('/'))
            };
            let covered = now
                .iter()
                .any(|(e, r)| within(e) && (r == "rw" || rights == "r"));
            if !covered {
                return Err(HostError::Denied);
            }
        }
        // the new view: the grants, and the always-present entries that are already in the view
        let mut view: Vec<(String, String)> = grants.to_vec();
        for keep in [".", "/dev", "/tmp"] {
            if view.iter().any(|(p, _)| p == keep) {
                continue;
            }
            if let Some(e) = now.iter().find(|(p, _)| p == keep) {
                view.push(e.clone());
            }
        }
        self.confined.push(view);
        Ok(())
    }

    fn unconfine(&mut self) {
        self.confined.pop();
    }

    fn kill(&mut self, pid: Pid) {
        if let Some(e) = self.statuses.get_mut(&pid.0) {
            *e = (137, self.now_ms);
        }
    }

    fn term_read(&mut self) -> TermIn {
        self.term_in.pop_front().unwrap_or(TermIn::Gone)
    }

    fn list_dir(&mut self, path: &str) -> Result<Vec<(String, bool)>, HostError> {
        let dir = norm(&self.cwd, path);
        let prefix = alloc::format!("{}/", dir.trim_end_matches('/'));
        let mut out: Vec<(String, bool)> = Vec::new();
        for k in self.files.keys() {
            if let Some(rest) = k.strip_prefix(&prefix) {
                let first = rest.split('/').next().unwrap_or("");
                let is_dir = rest.contains('/');
                if !out.iter().any(|(n, _)| n == first) {
                    out.push((String::from(first), is_dir));
                }
            }
        }
        if out.is_empty() {
            return Err(HostError::NotFound);
        }
        out.sort();
        Ok(out)
    }

    fn interrupted(&mut self) -> bool {
        if self.interrupt_after_polls > 0 {
            self.interrupt_after_polls -= 1;
            return self.interrupt_after_polls == 0;
        }
        false
    }

    fn environ(&self) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn cwd(&self) -> String {
        self.cwd.clone()
    }

    fn chdir(&mut self, path: &str) -> Result<(), HostError> {
        let full = norm(&self.cwd, path);
        let prefix = alloc::format!("{}/", full.trim_end_matches('/'));
        if full == "/" || self.files.keys().any(|k| k.starts_with(&prefix)) {
            self.cwd = full;
            Ok(())
        } else {
            Err(HostError::NotFound)
        }
    }
}

fn lines(input: &[u8]) -> Vec<String> {
    let t = String::from_utf8_lossy(input).into_owned();
    if t.is_empty() {
        return Vec::new();
    }
    t.strip_suffix('\n')
        .unwrap_or(&t)
        .split('\n')
        .map(String::from)
        .collect()
}

fn standard_programs() -> Vec<(&'static str, Prog)> {
    fn echo(c: &mut ProgCtx) -> i32 {
        let (nl, rest) = match c.args.get(1).map(String::as_str) {
            Some("-n") => (false, &c.args[2..]),
            _ => (true, &c.args[1..]),
        };
        c.out.extend_from_slice(rest.join(" ").as_bytes());
        if nl {
            c.out.push(b'\n');
        }
        0
    }
    fn cat(c: &mut ProgCtx) -> i32 {
        if c.args.len() == 1 {
            c.out = core::mem::take(&mut c.input);
            return 0;
        }
        let mut st = 0;
        for a in &c.args[1..] {
            let full = norm(c.cwd, a);
            match c.files.get(&full) {
                Some(b) => c.out.extend_from_slice(b),
                None => {
                    c.err
                        .extend_from_slice(alloc::format!("cat: {a}: not found\n").as_bytes());
                    st = 1;
                }
            }
        }
        st
    }
    fn tru(_: &mut ProgCtx) -> i32 {
        0
    }
    fn fals(_: &mut ProgCtx) -> i32 {
        1
    }
    fn seq(c: &mut ProgCtx) -> i32 {
        let n = |i: usize| c.args.get(i).and_then(|s| s.parse::<i64>().ok());
        let (a, b) = match (n(1), n(2)) {
            (Some(b), None) => (1, b),
            (Some(a), Some(b)) => (a, b),
            _ => return 2,
        };
        let mut out = String::new();
        for i in a..=b {
            out.push_str(&i.to_string());
            out.push('\n');
        }
        c.out.extend_from_slice(out.as_bytes());
        0
    }
    fn upper(c: &mut ProgCtx) -> i32 {
        c.out = String::from_utf8_lossy(&c.input)
            .to_uppercase()
            .into_bytes();
        0
    }
    fn count_lines(c: &mut ProgCtx) -> i32 {
        let n = lines(&c.input).len();
        c.out.extend_from_slice(alloc::format!("{n}\n").as_bytes());
        0
    }
    fn exit_with(c: &mut ProgCtx) -> i32 {
        c.args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2)
    }
    fn err(c: &mut ProgCtx) -> i32 {
        let m = c.args[1..].join(" ");
        c.err.extend_from_slice(m.as_bytes());
        c.err.push(b'\n');
        0
    }
    fn fail_msg(c: &mut ProgCtx) -> i32 {
        let m = c.args.get(1).cloned().unwrap_or_default();
        c.err.extend_from_slice(m.as_bytes());
        c.err.push(b'\n');
        c.args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1)
    }
    fn sort(c: &mut ProgCtx) -> i32 {
        let mut l = lines(&c.input);
        l.sort();
        for x in l {
            c.out.extend_from_slice(x.as_bytes());
            c.out.push(b'\n');
        }
        0
    }
    fn grep(c: &mut ProgCtx) -> i32 {
        let pat = c.args.get(1).cloned().unwrap_or_default();
        let mut any = false;
        for l in lines(&c.input) {
            if l.contains(&pat) {
                any = true;
                c.out.extend_from_slice(l.as_bytes());
                c.out.push(b'\n');
            }
        }
        if any {
            0
        } else {
            1
        }
    }
    fn ls(c: &mut ProgCtx) -> i32 {
        let dir = norm(c.cwd, c.args.get(1).map_or(".", String::as_str));
        let prefix = alloc::format!("{}/", dir.trim_end_matches('/'));
        let mut names: Vec<String> = Vec::new();
        for k in c.files.keys() {
            if let Some(rest) = k.strip_prefix(&prefix) {
                let first = rest.split('/').next().unwrap_or("");
                let n = if rest.contains('/') {
                    alloc::format!("{first}/")
                } else {
                    String::from(first)
                };
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
        if names.is_empty() {
            c.err.extend_from_slice(b"ls: not found\n");
            return 1;
        }
        names.sort();
        for n in names {
            c.out.extend_from_slice(n.as_bytes());
            c.out.push(b'\n');
        }
        0
    }
    fn loud(c: &mut ProgCtx) -> i32 {
        let n: usize = c.args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        for i in 0..n {
            c.out
                .extend_from_slice(alloc::format!("line {i}\n").as_bytes());
        }
        0
    }
    fn many_then_fail(c: &mut ProgCtx) -> i32 {
        // writes lines, then exits 141 like a program killed by SIGPIPE
        let n: usize = c.args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        for i in 0..n {
            c.out.extend_from_slice(alloc::format!("{i}\n").as_bytes());
        }
        141
    }
    fn show_table(c: &mut ProgCtx) -> i32 {
        for (p, r) in c.table {
            c.out
                .extend_from_slice(alloc::format!("{p} {r}\n").as_bytes());
        }
        0
    }
    fn printenv(c: &mut ProgCtx) -> i32 {
        let mut all: Vec<&(String, String)> = c.env.iter().collect();
        all.sort();
        for (k, v) in all {
            if c.args.len() == 1 || c.args[1..].contains(k) {
                c.out
                    .extend_from_slice(alloc::format!("{k}={v}\n").as_bytes());
            }
        }
        0
    }
    fn slow(c: &mut ProgCtx) -> i32 {
        // slow MS [STATUS]: ends after MS milliseconds of the fake clock
        c.run_ms = c.args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        c.out
            .extend_from_slice(alloc::format!("slow {} done\n", c.run_ms).as_bytes());
        c.args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0)
    }
    fn hang(c: &mut ProgCtx) -> i32 {
        c.run_ms = u64::MAX;
        c.out.extend_from_slice(b"hanging\n");
        0
    }
    vec_of(&[
        ("echo", echo as Prog),
        ("cat", cat),
        ("true", tru),
        ("false", fals),
        ("seq", seq),
        ("upper-stdin", upper),
        ("count-lines", count_lines),
        ("exit-with", exit_with),
        ("err", err),
        ("fail-msg", fail_msg),
        ("sort-lines", sort),
        ("grep", grep),
        ("ls", ls),
        ("loud", loud),
        ("many-then-fail", many_then_fail),
        ("printenv", printenv),
        ("show-table", show_table),
        ("slow", slow),
        ("hang", hang),
    ])
}

fn vec_of(a: &[(&'static str, Prog)]) -> Vec<(&'static str, Prog)> {
    a.to_vec()
}

//! Pipelines: external programs connected by host pipes, with in-process stages (`each`, `map`, `filter`, `fold`, `take`, or
//! any one shell function) at the end, and a shell callee allowed at the start.

use super::*;
use alloc::collections::VecDeque;
use alloc::rc::Rc;

type Drains = Vec<(Stream, Option<Rc<RefCell<Vec<u8>>>>)>;

/// What an in-process stream stage does with each line.
enum SStage {
    Map(Rc<Closure>),
    Filter(Rc<Closure>),
    Each(Rc<Closure>),
    Fold(Val, Rc<Closure>),
    Take(usize, usize),
}

enum Emit {
    Collect(Vec<Val>),
    Print,
}

enum LineSrc {
    Stream {
        s: Stream,
        buf: Vec<u8>,
        eof: bool,
        /// `lossy-lines`: invalid UTF-8 becomes U+FFFD instead of an error (A4).
        lossy: bool,
    },
    Lines(VecDeque<String>),
}

fn stream_stage(st: &Stage) -> Option<SStage> {
    let StageKind::Proc {
        name,
        args,
        closure: None,
    } = &st.kind
    else {
        return None;
    };
    if !st.redirs.is_empty() {
        return None;
    }
    let cl = |v: &Val| match v {
        Val::Closure(c) => Some(c.clone()),
        _ => None,
    };
    match (name.as_str(), args.as_slice()) {
        ("map", [f]) => Some(SStage::Map(cl(f)?)),
        ("filter", [f]) => Some(SStage::Filter(cl(f)?)),
        ("each", [f]) => Some(SStage::Each(cl(f)?)),
        ("fold", [init, f]) => Some(SStage::Fold(init.clone(), cl(f)?)),
        ("take", [Val::Num(Number::Int(n))]) if *n >= 0 => Some(SStage::Take(*n as usize, 0)),
        _ => None,
    }
}

impl<H: Host> Shell<H> {
    pub(crate) fn run_stages(&mut self, stages: Vec<Stage>, mode: Mode) -> Res<Val> {
        let n = stages.len();
        if n == 1 {
            let st = stages.into_iter().next().expect("one stage");
            return match st.kind {
                StageKind::Proc { .. } => self.call_proc(st, mode, None),
                StageKind::Ext(_) => self.run_ext_pipeline(vec![st], None, mode),
            };
        }
        for st in &stages {
            if let StageKind::Proc {
                name,
                closure: None,
                ..
            } = &st.kind
            {
                if matches!(name.as_str(), "cd" | "pwd" | "exit" | "help" | "halt") {
                    return Err(error(&format!("{name}: a builtin cannot be piped")));
                }
            }
        }
        // shape: [producer proc] ext* [consumer proc]*
        let mut it = stages.into_iter().peekable();
        let mut producer = None;
        if matches!(it.peek().map(|s| &s.kind), Some(StageKind::Proc { .. })) {
            producer = it.next();
        }
        let mut ext = Vec::new();
        while matches!(it.peek().map(|s| &s.kind), Some(StageKind::Ext(_))) {
            ext.push(it.next().expect("peeked"));
        }
        let consumers: Vec<Stage> = it.collect();
        if consumers
            .iter()
            .any(|s| matches!(s.kind, StageKind::Ext(_)))
        {
            return Err(error(
                "an in-process command (each, map, a function...) can only start or end a pipeline",
            ));
        }
        if producer.is_some() && ext.is_empty() && consumers.is_empty() {
            unreachable!("a one-stage pipeline is handled above");
        }
        // a producer's output is buffered (up to the capture cap) and becomes the input of what follows
        let mut feed: Option<Vec<u8>> = None;
        if let Some(p) = producer {
            let buf = Rc::new(RefCell::new(Vec::new()));
            let saved = core::mem::replace(&mut self.io.stdout, Sink::Mem(buf.clone()));
            let r = self.call_proc(p, Mode::Stmt, None);
            self.io.stdout = saved;
            r?;
            let bytes = core::mem::take(&mut *buf.borrow_mut());
            feed = Some(bytes);
        }
        if ext.is_empty() {
            // producer | consumers, all in process
            let bytes = feed.unwrap_or_default();
            let text = String::from_utf8(bytes).map_err(|_| error("output is not valid UTF-8"))?;
            let src = LineSrc::Lines(split_lines(&text).into());
            return self.run_consumers(src, consumers, mode);
        }
        if consumers.is_empty() {
            return self.run_ext_pipeline(ext, feed, mode);
        }
        self.run_ext_then_consumers(ext, feed, consumers, mode)
    }

    // ---- redirections ----

    pub(super) fn open_redir(&mut self, path: &str, kind: RedirKind) -> Res<Stream> {
        let m = match kind {
            RedirKind::Read => OpenMode::Read,
            RedirKind::Append => OpenMode::Append,
            _ => OpenMode::Write,
        };
        self.host
            .open(path, m)
            .map_err(|e| error(&format!("{path}: {}", e.message())))
    }

    /// Run an in-process stage (a builtin or a shell function) with its redirections.
    pub(super) fn call_proc(&mut self, st: Stage, mode: Mode, stdin: Option<Stream>) -> Res<Val> {
        let StageKind::Proc {
            name,
            args,
            closure,
        } = st.kind
        else {
            unreachable!("call_proc on an external stage")
        };
        let mut io = self.io.clone();
        if let Some(s) = stdin {
            io.stdin = s;
        }
        let mut opened = Vec::new();
        for (kind, fd, to, path) in &st.redirs {
            match kind {
                RedirKind::Read => match self.open_redir(path.as_deref().unwrap_or(""), *kind) {
                    Ok(s) => {
                        io.stdin = s;
                        opened.push(s);
                    }
                    Err(e) => {
                        self.close_all(&opened);
                        return Err(e);
                    }
                },
                RedirKind::Write | RedirKind::Append => {
                    match self.open_redir(path.as_deref().unwrap_or(""), *kind) {
                        Ok(s) => {
                            opened.push(s);
                            if *fd == 2 {
                                io.stderr = Sink::Host(s);
                            } else {
                                io.stdout = Sink::Host(s);
                            }
                        }
                        Err(e) => {
                            self.close_all(&opened);
                            return Err(e);
                        }
                    }
                }
                RedirKind::Dup => {
                    let src = if *to == 2 {
                        io.stderr.clone()
                    } else {
                        io.stdout.clone()
                    };
                    if *fd == 2 {
                        io.stderr = src;
                    } else if *fd == 1 {
                        io.stdout = src;
                    }
                }
            }
        }
        let saved = core::mem::replace(&mut self.io, io);
        let r = match closure {
            Some(c) => self.call_closure(&c, args),
            None => self.call_builtin(&name, args),
        };
        self.io = saved;
        self.close_all(&opened);
        let _ = mode;
        r
    }

    pub(super) fn close_all(&mut self, streams: &[Stream]) {
        for s in streams {
            self.host.close(*s);
        }
    }

    // ---- external stages ----

    /// Start the external stages with the given ends. Pipes between stages are made and closed here.
    pub(super) fn start_ext(
        &mut self,
        ext: &[Stage],
        first_in: Stream,
        last_out: Stream,
        err: Stream,
    ) -> Res<Vec<(Pid, String)>> {
        let env = self.child_env()?;
        let mut pids: Vec<(Pid, String)> = Vec::new();
        let mut prev_read: Option<Stream> = None;
        let n = ext.len();
        let mut failure: Option<Flow> = None;
        for (i, st) in ext.iter().enumerate() {
            let StageKind::Ext(argv) = &st.kind else {
                unreachable!()
            };
            let pipe_in = prev_read.take();
            let mut inp = pipe_in.unwrap_or(first_in);
            let mut outp;
            let mut next_read = None;
            let mut created: Vec<Stream> = Vec::new();
            if i + 1 < n {
                match self.host.pipe() {
                    Ok((r, w)) => {
                        outp = w;
                        next_read = Some(r);
                        created.push(w);
                    }
                    Err(e) => {
                        failure = Some(error(&format!("cannot make a pipe: {}", e.message())));
                        break;
                    }
                }
            } else {
                outp = last_out;
            }
            let mut errp = err;
            let mut opened = Vec::new();
            let mut bad = None;
            for (kind, fd, to, path) in &st.redirs {
                let slot = |fd: u8, i: Stream, o: Stream, e: Stream| match fd {
                    0 => i,
                    1 => o,
                    _ => e,
                };
                match kind {
                    RedirKind::Dup => {
                        let src = slot(*to, inp, outp, errp);
                        match fd {
                            1 => outp = src,
                            2 => errp = src,
                            _ => inp = src,
                        }
                    }
                    _ => match self.open_redir(path.as_deref().unwrap_or(""), *kind) {
                        Ok(s) => {
                            opened.push(s);
                            match (kind, fd) {
                                (RedirKind::Read, _) => inp = s,
                                (_, 2) => errp = s,
                                _ => outp = s,
                            }
                        }
                        Err(e) => {
                            bad = Some(e);
                            break;
                        }
                    },
                }
            }
            if let Some(e) = bad {
                self.close_all(&opened);
                self.close_all(&created);
                if let Some(r) = next_read {
                    self.host.close(r);
                }
                failure = Some(e);
                break;
            }
            let res = self.host.spawn(
                argv,
                &env,
                ProcIo {
                    stdin: inp,
                    stdout: outp,
                    stderr: errp,
                },
            );
            // our copies of the pipe ends and redirected files are no longer needed
            self.close_all(&opened);
            self.close_all(&created);
            if let Some(pr) = pipe_in {
                // the read end of the previous pipe
                self.host.close(pr);
            }
            match res {
                Ok(pid) => pids.push((pid, argv[0].clone())),
                Err(e) => {
                    if let Some(r) = next_read {
                        self.host.close(r);
                    }
                    let m = e.message();
                    failure = Some(Flow::Fail(Fail {
                        value: Val::record(vec![
                            ("kind", Val::str("spawn")),
                            ("command", Val::str(&argv[0])),
                            ("message", Val::string(m)),
                        ]),
                        code: 127,
                        kind: FailKind::Spawn,
                    }));
                    break;
                }
            }
            prev_read = next_read;
        }
        if let Some(f) = failure {
            if let Some(r) = prev_read {
                self.host.close(r);
            }
            for (pid, _) in &pids {
                self.host.kill(*pid);
            }
            for (pid, _) in &pids {
                self.host.wait(*pid);
            }
            return Err(f);
        }
        Ok(pids)
    }

    /// Wait for every stage; the first non-zero status is a failure (pipefail), unless `ignore` (we stopped reading early).
    pub(super) fn wait_all(&mut self, pids: &[(Pid, String)], ignore: bool) -> Res<()> {
        let mut first: Option<(i32, String)> = None;
        let mut all = Vec::new();
        for (pid, name) in pids {
            let st = self.host.wait(*pid);
            if st == crate::host::STOPPED_STATUS {
                // the terminal stopped the job: it is in the host's job table now, and the rest of the stages with it
                return Ok(());
            }
            all.push(Val::int(i64::from(st)));
            if st != 0 && first.is_none() {
                first = Some((st, name.clone()));
            }
        }
        match first {
            Some((st, name)) if !ignore => Err(Flow::Fail(Fail {
                value: Val::record(vec![
                    ("kind", Val::str("exit")),
                    ("status", Val::int(i64::from(st))),
                    ("command", Val::string(name)),
                    ("statuses", Val::list(all)),
                ]),
                code: st,
                kind: FailKind::Exit,
            })),
            _ => Ok(()),
        }
    }

    /// The pipeline's stages are running: make them the foreground job. On failure they are killed.
    pub(super) fn begin_job(&mut self, pids: &[(Pid, String)]) -> Res<()> {
        let ps: Vec<Pid> = pids.iter().map(|(p, _)| *p).collect();
        if self.host.started(&ps) {
            return Ok(());
        }
        for p in &ps {
            self.host.kill(*p);
        }
        for p in &ps {
            self.host.wait(*p);
        }
        self.host.finished();
        Err(error("could not register the job"))
    }

    /// Read every listed stream to its end into its buffer (None discards), without deadlock between them.
    /// Returns true if a buffer hit the capture cap (that stream was closed).
    pub(super) fn drain_all(&mut self, mut reads: Drains) -> bool {
        let mut capped = false;
        let mut chunk = [0u8; 4096];
        while !reads.is_empty() {
            let streams: Vec<Stream> = reads.iter().map(|(s, _)| *s).collect();
            match self.host.poll_read(&streams, &mut chunk) {
                Ok((i, 0)) => {
                    let (s, _) = reads.remove(i);
                    self.host.close(s);
                }
                Ok((i, n)) => {
                    if let Some(buf) = &reads[i].1 {
                        let mut b = buf.borrow_mut();
                        if b.len() + n > self.cap {
                            drop(b);
                            capped = true;
                            let (s, _) = reads.remove(i);
                            self.host.close(s);
                            continue;
                        }
                        b.extend_from_slice(&chunk[..n]);
                    }
                }
                Err(_) => {
                    let (s, _) = reads.remove(0);
                    self.host.close(s);
                }
            }
        }
        capped
    }

    /// The streams that stderr and (when not capturing) stdout of the last external stage go to: either the shell's own, or
    /// pipes that are drained into memory.
    pub(super) fn sink_stream(
        &mut self,
        sink: &Sink,
        drains: &mut Drains,
        to_close: &mut Vec<Stream>,
    ) -> Res<Stream> {
        match sink {
            Sink::Host(s) => Ok(*s),
            Sink::Mem(m) => {
                let (r, w) = self
                    .host
                    .pipe()
                    .map_err(|e| error(&format!("cannot make a pipe: {}", e.message())))?;
                drains.push((r, Some(m.clone())));
                to_close.push(w);
                Ok(w)
            }
            Sink::Null => {
                let (r, w) = self
                    .host
                    .pipe()
                    .map_err(|e| error(&format!("cannot make a pipe: {}", e.message())))?;
                drains.push((r, None));
                to_close.push(w);
                Ok(w)
            }
        }
    }

    /// Feed bytes to a new pipe and return its read end (the write end is closed: the reader sees them and then the end).
    pub(super) fn feed_stream(&mut self, bytes: &[u8]) -> Res<Stream> {
        let (r, w) = self
            .host
            .pipe()
            .map_err(|e| error(&format!("cannot make a pipe: {}", e.message())))?;
        let res = self.host.write(w, bytes);
        self.host.close(w);
        res.map_err(|e| error(&format!("cannot feed the pipe: {}", e.message())))?;
        Ok(r)
    }

    fn run_ext_pipeline(&mut self, ext: Vec<Stage>, feed: Option<Vec<u8>>, mode: Mode) -> Res<Val> {
        let capture = mode == Mode::Value;
        let mut drains = Vec::new();
        let mut to_close = Vec::new();
        let first_in = match &feed {
            Some(b) => self.feed_stream(b)?,
            None => self.io.stdin,
        };
        let cap_buf = Rc::new(RefCell::new(Vec::new()));
        let out_sink = if capture {
            Sink::Mem(cap_buf.clone())
        } else {
            self.io.stdout.clone()
        };
        let err_sink = self.io.stderr.clone();
        let last_out = self.sink_stream(&out_sink, &mut drains, &mut to_close)?;
        let err = self.sink_stream(&err_sink, &mut drains, &mut to_close)?;
        let started = self.start_ext(&ext, first_in, last_out, err);
        self.close_all(&to_close);
        if feed.is_some() {
            self.host.close(first_in);
        }
        let pids = started.inspect_err(|_| {
            let rs: Vec<Stream> = drains.iter().map(|(s, _)| *s).collect();
            for s in rs {
                self.host.close(s);
            }
        })?;
        if let Err(e) = self.begin_job(&pids) {
            let rs: Vec<Stream> = drains.iter().map(|(s, _)| *s).collect();
            self.close_all(&rs);
            return Err(e);
        }
        let capped = self.drain_all(drains);
        let waited = self.wait_all(&pids, capped);
        self.host.finished();
        if capped {
            return Err(Flow::Fail(Fail {
                value: err_val("capture-limit", "output too large to capture"),
                code: 1,
                kind: FailKind::CaptureLimit,
            }));
        }
        waited?;
        if capture {
            let text = String::from_utf8(core::mem::take(&mut *cap_buf.borrow_mut()))
                .map_err(|_| error("output is not valid UTF-8"))?;
            Ok(split_lines_val(&text))
        } else {
            Ok(Val::Null)
        }
    }

    fn run_ext_then_consumers(
        &mut self,
        ext: Vec<Stage>,
        feed: Option<Vec<u8>>,
        consumers: Vec<Stage>,
        mode: Mode,
    ) -> Res<Val> {
        let mut drains = Vec::new();
        let mut to_close = Vec::new();
        let first_in = match &feed {
            Some(b) => self.feed_stream(b)?,
            None => self.io.stdin,
        };
        let (rd, wr) = self
            .host
            .pipe()
            .map_err(|e| error(&format!("cannot make a pipe: {}", e.message())))?;
        let err_sink = self.io.stderr.clone();
        let err = self.sink_stream(&err_sink, &mut drains, &mut to_close)?;
        let started = self.start_ext(&ext, first_in, wr, err);
        self.host.close(wr);
        self.close_all(&to_close);
        if feed.is_some() {
            self.host.close(first_in);
        }
        let pids = match started {
            Ok(p) => p,
            Err(e) => {
                self.host.close(rd);
                let rs: Vec<Stream> = drains.iter().map(|(s, _)| *s).collect();
                self.close_all(&rs);
                return Err(e);
            }
        };
        let src = LineSrc::Stream {
            s: rd,
            buf: Vec::new(),
            eof: false,
            lossy: false,
        };
        let (res, stopped_early) = self.run_consumers_inner(src, consumers, mode);
        self.host.close(rd);
        self.drain_all(drains);
        let waited = self.wait_all(&pids, stopped_early);
        self.host.finished();
        let v = res?;
        waited?;
        Ok(v)
    }

    // ---- in-process consumers ----

    fn run_consumers(&mut self, src: LineSrc, consumers: Vec<Stage>, mode: Mode) -> Res<Val> {
        let (r, _) = self.run_consumers_inner(src, consumers, mode);
        r
    }

    /// Returns the value and whether input reading stopped early (a `take` was satisfied).
    fn run_consumers_inner(
        &mut self,
        mut src: LineSrc,
        consumers: Vec<Stage>,
        mode: Mode,
    ) -> (Res<Val>, bool) {
        // `lossy-lines` first in the consumers makes the line reading forgiving about invalid UTF-8 (A4)
        let mut consumers = consumers;
        let mut lossy_first = false;
        if matches!(consumers.first(), Some(Stage { kind: StageKind::Proc { name, args, closure: None }, redirs }) if name == "lossy-lines" && args.is_empty() && redirs.is_empty())
        {
            consumers.remove(0);
            lossy_first = true;
            if let LineSrc::Stream { lossy, .. } = &mut src {
                *lossy = true;
            }
        }
        let chain: Option<Vec<SStage>> = consumers.iter().map(stream_stage).collect();
        match chain {
            Some(stages) if !stages.is_empty() || lossy_first => {
                self.run_chain(&mut src, stages, mode)
            }
            _ => {
                // a single arbitrary shell function reading standard input
                if consumers.len() != 1 {
                    return (
                        Err(error(
                            "only each, map, filter, fold and take can be chained after a pipe",
                        )),
                        false,
                    );
                }
                let st = consumers.into_iter().next().expect("one");
                let stream = match &mut src {
                    LineSrc::Stream { s, .. } => *s,
                    LineSrc::Lines(l) => {
                        let mut text = String::new();
                        for x in l.iter() {
                            text.push_str(x);
                            text.push('\n');
                        }
                        match self.feed_stream(text.as_bytes()) {
                            Ok(s) => s,
                            Err(e) => return (Err(e), false),
                        }
                    }
                };
                let r = self.call_proc(st, mode, Some(stream));
                (r, false)
            }
        }
    }

    fn next_line(&mut self, src: &mut LineSrc) -> Res<Option<String>> {
        match src {
            LineSrc::Lines(l) => Ok(l.pop_front()),
            LineSrc::Stream { s, buf, eof, lossy } => loop {
                let decode = |line: Vec<u8>| -> Res<Option<String>> {
                    if *lossy {
                        Ok(Some(String::from_utf8_lossy(&line).into_owned()))
                    } else {
                        String::from_utf8(line).map(Some).map_err(|_| error("input is not valid UTF-8 (put `lossy-lines` before the first stage to replace bad bytes)"))
                    }
                };
                if let Some(i) = buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=i).take(i).collect();
                    return decode(line);
                }
                if *eof {
                    if buf.is_empty() {
                        return Ok(None);
                    }
                    let line = core::mem::take(buf);
                    return decode(line);
                }
                let mut chunk = [0u8; 4096];
                match self.host.read(*s, &mut chunk) {
                    Ok(0) => *eof = true,
                    Ok(n) => {
                        if buf.len() + n > 1 << 20 && !buf.contains(&b'\n') {
                            return Err(error("line too long"));
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    Err(e) => return Err(error(&format!("read failed: {}", e.message()))),
                }
            },
        }
    }

    fn run_chain(
        &mut self,
        src: &mut LineSrc,
        mut stages: Vec<SStage>,
        mode: Mode,
    ) -> (Res<Val>, bool) {
        let mut emit = if mode == Mode::Value {
            Emit::Collect(Vec::new())
        } else {
            Emit::Print
        };
        let last_is_fold = matches!(stages.last(), Some(SStage::Fold(..)));
        let last_is_each = matches!(stages.last(), Some(SStage::Each(..)));
        let mut stopped = false;
        let r: Res<()> = (|| {
            while let Some(line) = self.next_line(src)? {
                if self.push_item(&mut stages, 0, Val::string(line), &mut emit)? {
                    stopped = true;
                    break;
                }
            }
            // end of input: folds hand over their result
            for i in 0..stages.len() {
                if let SStage::Fold(acc, _) = &stages[i] {
                    let v = acc.clone();
                    self.push_item(&mut stages, i + 1, v, &mut emit)?;
                }
            }
            Ok(())
        })();
        if let Err(e) = r {
            return (Err(e), stopped);
        }
        let v = match emit {
            Emit::Print => Val::Null,
            Emit::Collect(mut items) => {
                if last_is_each {
                    Val::Null
                } else if last_is_fold {
                    items.pop().unwrap_or(Val::Null)
                } else {
                    Val::list(items)
                }
            }
        };
        (Ok(v), stopped)
    }

    /// Send one item into stage `i`. Returns true when no more input is wanted.
    fn push_item(
        &mut self,
        stages: &mut Vec<SStage>,
        i: usize,
        item: Val,
        emit: &mut Emit,
    ) -> Res<bool> {
        if i >= stages.len() {
            match emit {
                Emit::Collect(v) => v.push(item),
                Emit::Print => {
                    let mut text = crate::builtins::echo_line(&item);
                    text.push('\n');
                    self.out(text.as_bytes())?;
                }
            }
            return Ok(false);
        }
        match &mut stages[i] {
            SStage::Map(f) => {
                let f = f.clone();
                let v = self.call_closure(&f, vec![item])?;
                self.push_item(stages, i + 1, v, emit)
            }
            SStage::Filter(f) => {
                let f = f.clone();
                match self.call_closure(&f, vec![item.clone()])? {
                    Val::Bool(true) => self.push_item(stages, i + 1, item, emit),
                    Val::Bool(false) => Ok(false),
                    v => Err(error(&format!(
                        "filter needs a boolean, got a {}",
                        v.kind()
                    ))),
                }
            }
            SStage::Each(f) => {
                let f = f.clone();
                self.call_closure(&f, vec![item])?;
                Ok(false)
            }
            SStage::Fold(acc, f) => {
                let f = f.clone();
                let a = core::mem::replace(acc, Val::Null);
                let nv = self.call_closure(&f, vec![a, item])?;
                if let SStage::Fold(acc, _) = &mut stages[i] {
                    *acc = nv;
                }
                Ok(false)
            }
            SStage::Take(n, taken) => {
                if *taken >= *n {
                    return Ok(true);
                }
                *taken += 1;
                let reached = *taken >= *n;
                let stop_below = self.push_item(stages, i + 1, item, emit)?;
                Ok(reached || stop_below)
            }
        }
    }
}

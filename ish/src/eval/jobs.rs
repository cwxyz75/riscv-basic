//! The command prefixes (C4: `succeeds`, `run`, `spawn`, `with-env`) and background jobs (J1 to J10).

use super::*;
use crate::host::{Pid, Stream, WaitOutcome};
use crate::job::{JobState, MemBuf};

fn text_of(v: &Val) -> Option<String> {
    v.word()
}

impl<H: Host> Shell<H> {
    pub(super) fn exec_prefixed(&mut self, p: &Prefix, stages: &[Command], mode: Mode) -> Res<Val> {
        match p.kind {
            PrefixKind::Succeeds => match self.exec_stages(stages, Mode::Stmt) {
                Ok(_) => Ok(Val::Bool(true)),
                Err(Flow::Fail(f)) if is_test_failure(&f) => Ok(Val::Bool(false)),
                Err(e) => Err(e),
            },
            PrefixKind::WithEnv => {
                let Some(arg) = &p.arg else {
                    return Err(error("with-env needs a map of variables"));
                };
                let Val::Map(map) = self.eval_expr(arg)? else {
                    return Err(error("with-env needs a map of variables"));
                };
                let mut extra = Vec::new();
                for (k, v) in map.iter() {
                    match text_of(v) {
                        Some(t) => extra.push((String::from(k), t)),
                        None => {
                            return Err(error(&format!(
                                "with-env: `{k}` is a {}, not text",
                                v.kind()
                            )))
                        }
                    }
                }
                let saved = self.env_extra.clone();
                for e in extra {
                    self.env_extra.retain(|(n, _)| *n != e.0);
                    self.env_extra.push(e);
                }
                let r = self.exec_stages(stages, mode);
                self.env_extra = saved;
                r
            }
            PrefixKind::Run => {
                let opts = self.prefix_options(p, &["out", "err", "max", "timeout"])?;
                self.run_capture(stages, &opts)
            }
            PrefixKind::Spawn => {
                let opts = self.prefix_options(p, &["out", "err", "max", "timeout", "detach"])?;
                self.spawn_job(stages, &opts)
            }
        }
    }

    fn prefix_options(&mut self, p: &Prefix, allowed: &[&str]) -> Res<VMap> {
        let Some(arg) = &p.arg else {
            return Ok(VMap::new());
        };
        match self.eval_expr(arg)? {
            Val::Map(m) => {
                for (k, _) in m.iter() {
                    if !allowed.contains(&k) {
                        return Err(error(&format!("unknown option `{k}`")));
                    }
                }
                Ok((*m).clone())
            }
            v => Err(error(&format!("options must be a map, got a {}", v.kind()))),
        }
    }

    fn opt_mode(opts: &VMap, key: &str) -> Res<&'static str> {
        match opts.get(key) {
            None => Ok("capture"),
            Some(Val::Str(s)) => match &**s {
                "capture" => Ok("capture"),
                "inherit" => Ok("inherit"),
                "discard" => Ok("discard"),
                _ => Err(error(&format!(
                    "`{key}` must be \"capture\", \"inherit\" or \"discard\""
                ))),
            },
            Some(_) => Err(error(&format!(
                "`{key}` must be \"capture\", \"inherit\" or \"discard\""
            ))),
        }
    }

    fn opt_ms(opts: &VMap) -> Res<Option<u64>> {
        match opts.get("timeout") {
            None => Ok(None),
            Some(Val::Num(Number::Int(i))) if *i >= 0 => Ok(Some(*i as u64)),
            Some(_) => Err(error("`timeout` must be a number of milliseconds")),
        }
    }

    fn result_map(status: i32, out: &str, err: &str, value: Option<Val>) -> Val {
        let mut rec = vec![("status", Val::int(i64::from(status)))];
        match value {
            Some(v) => rec.push(("value", v)),
            None => rec.push(("out", split_lines_val(out))),
        }
        rec.push(("err", Val::str(err)));
        Val::record(rec)
    }

    /// `run [options] cmd`: capture and never raise for a non-zero exit (J3).
    fn run_capture(&mut self, stages: &[Command], opts: &VMap) -> Res<Val> {
        if let Some(ms) = Self::opt_ms(opts)? {
            // a bounded run is a job waited for with a timeout, killed when it runs out
            let job = self.spawn_job(stages, opts)?;
            let Val::Job(j) = &job else {
                return Err(error("run with a timeout needs a program"));
            };
            return match self.wait_job(j, Some(ms)) {
                Err(Flow::Fail(f)) if f.kind == FailKind::Timeout => {
                    self.kill_job(j);
                    let _ = self.finish_job(j);
                    Err(Flow::Fail(f))
                }
                r => r,
            };
        }
        let (om, em) = (Self::opt_mode(opts, "out")?, Self::opt_mode(opts, "err")?);
        let out = Rc::new(RefCell::new(Vec::new()));
        let err = Rc::new(RefCell::new(Vec::new()));
        let saved_cap = self.cap;
        if let Some(Val::Num(Number::Int(i))) = opts.get("max") {
            if *i > 0 {
                self.cap = *i as usize;
            }
        }
        let mut new = self.io.clone();
        match om {
            "capture" => new.stdout = Sink::Mem(out.clone()),
            "discard" => new.stdout = Sink::Null,
            _ => {}
        }
        match em {
            "capture" => new.stderr = Sink::Mem(err.clone()),
            "discard" => new.stderr = Sink::Null,
            _ => {}
        }
        let saved = core::mem::replace(&mut self.io, new);
        let r = self.exec_stages(stages, Mode::Stmt);
        self.io = saved;
        self.cap = saved_cap;
        let text = |b: &MemBuf| String::from_utf8_lossy(&b.borrow()).into_owned();
        let (status, value, failed) = match r {
            Ok(v) => (0, v, None),
            Err(Flow::Fail(f)) if is_test_failure(&f) => (f.code, Val::Null, Some(f)),
            Err(e) => return Err(e),
        };
        let out_text = text(&out);
        let value = if out_text.is_empty()
            && !matches!(value, Val::Null | Val::Void(_))
            && failed.is_none()
        {
            Some(value)
        } else {
            None
        };
        Ok(Self::result_map(status, &out_text, &text(&err), value))
    }

    // ---- jobs ----

    fn mem(&self) -> MemBuf {
        Rc::new(RefCell::new(Vec::new()))
    }

    /// `spawn [options] cmd`: start the rest of the pipeline in the background and return a job (J1, J2).
    pub(crate) fn spawn_job(&mut self, stages: &[Command], opts: &VMap) -> Res<Val> {
        let (om, em) = (
            if opts.get("out").is_some() {
                Self::opt_mode(opts, "out")?
            } else {
                "inherit"
            },
            if opts.get("err").is_some() {
                Self::opt_mode(opts, "err")?
            } else {
                "inherit"
            },
        );
        let detach = matches!(opts.get("detach"), Some(Val::Bool(true)));
        let mut resolved = Vec::new();
        for c in stages {
            resolved.push(self.resolve_stage(c)?);
        }
        let all_ext = resolved.iter().all(|s| matches!(s.kind, StageKind::Ext(_)));
        let state = if all_ext {
            let out_buf = (om == "capture").then(|| self.mem());
            let err_buf = (em == "capture").then(|| self.mem());
            let mut drains = Vec::new();
            let mut to_close = Vec::new();
            let out_sink = match om {
                "capture" => Sink::Mem(out_buf.clone().expect("capture has a buffer")),
                "discard" => Sink::Null,
                _ => self.io.stdout.clone(),
            };
            let err_sink = match em {
                "capture" => Sink::Mem(err_buf.clone().expect("capture has a buffer")),
                "discard" => Sink::Null,
                _ => self.io.stderr.clone(),
            };
            let last_out = self.sink_stream(&out_sink, &mut drains, &mut to_close)?;
            let err = self.sink_stream(&err_sink, &mut drains, &mut to_close)?;
            let stdin = self.io.stdin;
            let started = self.start_ext(&resolved, stdin, last_out, err);
            self.close_all(&to_close);
            let pids = match started {
                Ok(p) => p,
                Err(e) => {
                    let rs: Vec<Stream> = drains.iter().map(|(s, _)| *s).collect();
                    self.close_all(&rs);
                    return Err(e);
                }
            };
            JobState {
                statuses: vec![None; pids.len()],
                pids,
                drains,
                out: out_buf,
                err: err_buf,
                result: None,
                detach,
                cap: self.cap,
            }
        } else if resolved.len() == 1 {
            // a shell command: run now, to the end (there are no threads in the shell yet: work/)
            let out = self.mem();
            let err = self.mem();
            let mut new = self.io.clone();
            match om {
                "capture" => new.stdout = Sink::Mem(out.clone()),
                "discard" => new.stdout = Sink::Null,
                _ => {}
            }
            match em {
                "capture" => new.stderr = Sink::Mem(err.clone()),
                "discard" => new.stderr = Sink::Null,
                _ => {}
            }
            let st = resolved.into_iter().next().expect("one stage");
            let saved = core::mem::replace(&mut self.io, new);
            let r = self.call_proc(st, Mode::Stmt, None);
            self.io = saved;
            let text = |b: &MemBuf| String::from_utf8_lossy(&b.borrow()).into_owned();
            let (status, value) = match r {
                Ok(v) => (
                    0,
                    if matches!(v, Val::Null | Val::Void(_)) {
                        None
                    } else {
                        Some(v)
                    },
                ),
                Err(Flow::Fail(f)) if f.kind == FailKind::Exit => (f.code, None),
                Err(e) => return Err(e),
            };
            let value = if text(&out).is_empty() { value } else { None };
            JobState {
                pids: Vec::new(),
                statuses: Vec::new(),
                drains: Vec::new(),
                out: None,
                err: None,
                result: Some(Self::result_map(status, &text(&out), &text(&err), value)),
                detach,
                cap: self.cap,
            }
        } else {
            return Err(error(
                "spawn takes a pipeline of programs or one shell command",
            ));
        };
        let job = Rc::new(RefCell::new(state));
        self.jobs.push(job.clone());
        Ok(Val::Job(job))
    }

    /// Collect a job that has ended: drain what it wrote, reap every stage, make the result map.
    pub(crate) fn finish_job(&mut self, j: &Rc<RefCell<JobState>>) -> Res<Val> {
        if let Some(r) = j.borrow().result.clone() {
            return Ok(r);
        }
        let (drains, pids, known, out, err, cap) = {
            let mut s = j.borrow_mut();
            (
                core::mem::take(&mut s.drains),
                s.pids.clone(),
                s.statuses.clone(),
                s.out.clone(),
                s.err.clone(),
                s.cap,
            )
        };
        let saved_cap = self.cap;
        self.cap = cap;
        let capped = self.drain_all(drains);
        self.cap = saved_cap;
        let mut statuses = Vec::new();
        for (i, (pid, _)) in pids.iter().enumerate() {
            statuses.push(match known[i] {
                Some(st) => st,
                None => self.host.wait(*pid),
            });
        }
        if capped {
            return Err(Flow::Fail(Fail {
                value: err_val("capture-limit", "output too large to capture"),
                code: 1,
                kind: FailKind::CaptureLimit,
            }));
        }
        let status = statuses.iter().copied().find(|&s| s != 0).unwrap_or(0);
        let text = |b: &Option<MemBuf>| {
            b.as_ref()
                .map(|m| String::from_utf8_lossy(&m.borrow()).into_owned())
                .unwrap_or_default()
        };
        let r = Self::result_map(status, &text(&out), &text(&err), None);
        j.borrow_mut().result = Some(r.clone());
        Ok(r)
    }

    fn timeout_failure(ms: u64) -> Flow {
        Flow::Fail(Fail {
            value: err_val("timeout", &format!("timed out after {ms} ms")),
            code: 124,
            kind: FailKind::Timeout,
        })
    }

    /// `wait`: the job's result (J3). With a timeout it raises `timeout` and leaves the job running (J7). Waiting is
    /// interruptible (J9).
    pub(crate) fn wait_job(&mut self, j: &Rc<RefCell<JobState>>, timeout: Option<u64>) -> Res<Val> {
        if let Some(r) = j.borrow().result.clone() {
            return Ok(r);
        }
        let last = {
            let s = j.borrow();
            s.pids.last().map(|(p, _)| *p)
        };
        if let Some(main) = last {
            let idx = j.borrow().pids.len() - 1;
            if j.borrow().statuses[idx].is_none() {
                match timeout {
                    Some(ms) => match self.host.wait_timeout(main, ms) {
                        WaitOutcome::Done(st) => j.borrow_mut().statuses[idx] = Some(st),
                        WaitOutcome::Timeout => return Err(Self::timeout_failure(ms)),
                        WaitOutcome::Interrupted => return Err(self.interrupt_failure()),
                    },
                    None => loop {
                        // slices, so an interrupt can end the wait
                        match self.host.wait_timeout(main, 100) {
                            WaitOutcome::Done(st) => {
                                j.borrow_mut().statuses[idx] = Some(st);
                                break;
                            }
                            WaitOutcome::Timeout => self.poll_interrupt()?,
                            WaitOutcome::Interrupted => return Err(self.interrupt_failure()),
                        }
                    },
                }
            }
        }
        self.finish_job(j)
    }

    pub(crate) fn interrupt_failure(&mut self) -> Flow {
        self.interrupting = true;
        Flow::Fail(Fail {
            value: Val::str("interrupted"),
            code: 130,
            kind: FailKind::Interrupt,
        })
    }

    /// `poll`: `null` while the job runs, its result once it has ended (J5).
    pub(crate) fn poll_job(&mut self, j: &Rc<RefCell<JobState>>) -> Res<Val> {
        if let Some(r) = j.borrow().result.clone() {
            return Ok(r);
        }
        let main = {
            let s = j.borrow();
            s.pids.last().map(|(p, _)| *p)
        };
        if let Some(main) = main {
            let idx = j.borrow().pids.len() - 1;
            if j.borrow().statuses[idx].is_none() {
                match self.host.try_wait(main) {
                    Some(st) => j.borrow_mut().statuses[idx] = Some(st),
                    None => return Ok(Val::Null),
                }
            }
        }
        self.finish_job(j)
    }

    /// `wait-any`: the first of the jobs to end, as `{ "job": ..., "result": ... }` (J4).
    pub(crate) fn wait_any_jobs(&mut self, jobs: &[Rc<RefCell<JobState>>]) -> Res<Val> {
        if jobs.is_empty() {
            return Err(error("wait-any needs at least one job"));
        }
        // one that has already ended wins at once
        for j in jobs {
            if j.borrow().result.is_some() {
                let r = self.finish_job(j)?;
                return Ok(Val::record(vec![
                    ("job", Val::Job(j.clone())),
                    ("result", r),
                ]));
            }
        }
        let mut pids = Vec::new();
        let mut which = Vec::new();
        for (i, j) in jobs.iter().enumerate() {
            if let Some((p, _)) = j.borrow().pids.last() {
                pids.push(*p);
                which.push(i);
            }
        }
        let (k, st) = self.host.wait_any(&pids);
        let jref = &jobs[which[k]];
        {
            let mut s = jref.borrow_mut();
            let idx = s.pids.len() - 1;
            s.statuses[idx] = Some(st);
        }
        let r = self.finish_job(jref)?;
        Ok(Val::record(vec![
            ("job", Val::Job(jref.clone())),
            ("result", r),
        ]))
    }

    /// `kill`: end the job (J6).
    pub(crate) fn kill_job(&mut self, j: &Rc<RefCell<JobState>>) {
        let pids: Vec<Pid> = j.borrow().pids.iter().map(|(p, _)| *p).collect();
        for p in pids {
            self.host.kill(p);
        }
    }

    /// When a script ends, for any reason, its unfinished jobs are killed unless detached (J8).
    pub(crate) fn kill_jobs(&mut self) {
        let jobs = core::mem::take(&mut self.jobs);
        for j in jobs {
            let (done, detach) = {
                let s = j.borrow();
                (s.result.is_some(), s.detach)
            };
            if done || detach {
                continue;
            }
            self.kill_job(&j);
            let _ = self.finish_job(&j);
        }
    }
}

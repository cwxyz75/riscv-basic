//! The session manager's code (UV5, UV5a, UV7; T128): profiles, the stack of services one session is made of, the login
//! prompts and the boot policy. It is compiled into `sessiond` (which runs it in production) and into `init` (whose test aids,
//! enabled by the `repl` argument, build sessions themselves). Nothing here uses authority only `init` has: what it needs from
//! the kernel's first process comes in as [`Powers`].
#![allow(dead_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use proto::startup::Role;
use rt::io;

/// A named entry of the boot image.
pub fn image_entry<'a>(
    img: &'a [u8],
    entries: &[proto::bootimg::Entry],
    name: &[u8],
) -> Option<&'a [u8]> {
    let e = entries.iter().find(|e| e.name == name)?;
    img.get(e.offset as usize..(e.offset + e.size) as usize)
}

/// A profile as read from the image, with its text (the shell is given it to read its own grants).
#[derive(Clone)]
pub struct Loaded {
    pub profile: profile::Profile,
    pub text: String,
}

/// Every profile in the boot image's `etc/profiles/` (a profile that does not parse is skipped, and said so).
pub fn load_profiles(img: &[u8], entries: &[proto::bootimg::Entry]) -> Vec<Loaded> {
    let mut out = Vec::new();
    for e in entries
        .iter()
        .filter(|e| e.name.starts_with(b"etc/profiles/") && e.name.ends_with(b".json"))
    {
        let Some(bytes) = image_entry(img, entries, &e.name) else {
            continue;
        };
        let text = String::from_utf8_lossy(bytes).into_owned();
        match profile::Profile::parse(&text) {
            Ok(p) => out.push(Loaded { profile: p, text }),
            Err(err) => {
                rt::sys::debug_write(
                    alloc::format!("init: {}: {err}\n", String::from_utf8_lossy(&e.name))
                        .as_bytes(),
                );
            }
        }
    }
    out
}

/// Where a session's terminal comes from: the serial line (through a console server), or a socket the caller holds (no
/// console server: the shell runs a script from it, and the session has no volumes).
#[derive(Clone, Copy)]
pub enum Term {
    Serial(u32),
    /// A byte socket a backend provider holds the other end of (UV7): the console server serves it, treats its closing as a
    /// detach and takes a new one on the stack's `term` channel.
    Backend(u32),
    Socket(u32),
}

/// The services of one interactive session, in start order.
pub struct Stack {
    /// The job every process of the session lives in (handle with every right): `stop_stack` ends
    /// whatever is left in it, and its limits are the whole session's.
    pub session: u32,
    pub memfsd: u32,
    pub launcher: u32,
    pub consoled: Option<u32>,
    pub sh: u32,
    /// `sh` writes here when the user says `halt`.
    pub halt: Option<u32>,
    /// A connection to the service registry, to ask the volumes to commit before the session ends.
    pub registry: Option<rt::fs::Fs>,
    /// Init's end of the console server's attach channel (a `Term::Backend` session): `proto::console::term`.
    pub term: Option<u32>,
}

pub fn kill_close(h: u32) {
    rt::sys::process_kill(h, -1);
    rt::sys::handle_close(h);
}

/// The authority a session manager needs from `init` and nothing the kernel gives only to its first process: a handle to the
/// root job (to make each session's job) and to the root resource (for the device manager).
pub struct Powers {
    pub root_job: u32,
    pub resource: u32,
}

pub fn start_stack(
    powers: &Powers,
    term: Term,
    image_vmo: u32,
    img: &[u8],
    entries: &[proto::bootimg::Entry],
    who: &Loaded,
) -> Result<Stack, i32> {
    use proto::startup::Startup;
    use rt::abi::rights::Rights;
    use rt::sys;
    let chan = || -> Result<(u32, u32), i32> {
        let c = sys::channel_create();
        if c.status != 0 {
            return Err(30);
        }
        Ok((c.a1 as u32, c.a2 as u32))
    };
    // The session's job, and under it the job the launcher makes every program's job in (S4.5).
    // Limits are generous for the services and bounded for programs: all together at most 64 MiB
    // of memory, 32 processes, 64 threads, 4000 objects and 1.5 MiB of queued IPC data; programs
    // all together at most 16 processes, 32 MiB, 1000 objects and 768 KiB queued.
    let root = sys::handle_duplicate(powers.root_job, Rights::SAME.0);
    if root.status != 0 {
        return Err(38);
    }
    let root = root.a1 as u32;
    let session = sys::job_create(root, &[32, 64, 49152, 40, 4000, 1536 * 1024]);
    sys::handle_close(root);
    if session.status != 0 {
        return Err(38);
    }
    let session = session.a1 as u32;
    // (a profile's quota can only lower what programs may have)
    let (app_procs, app_pages) = match &who.profile.quota {
        Some(q) => (q.processes.min(16), (q.memory_mib * 256).min(40960)),
        None => (16, 40960),
    };
    let apps = sys::job_create(session, &[app_procs, 32, app_pages, 20, 1000, 768 * 1024]);
    if apps.status != 0 {
        sys::handle_close(session);
        return Err(38);
    }
    let apps = apps.a1 as u32;
    let start_env = |name: &[u8],
                     roles: Vec<(Role, u32)>,
                     handles: &[u32],
                     extra: Vec<Vec<u8>>|
     -> Result<u32, i32> {
        let path: Vec<u8> = [&b"bin/"[..], name].concat();
        let Some(image) = image_entry(img, entries, &path) else {
            for &h in handles {
                sys::handle_close(h);
            }
            return Err(31);
        };
        let st = Startup {
            args: alloc::vec![name.to_vec()],
            roles,
            // (the services that log their start-up to the serial line stay quiet in a user's session)
            env: [alloc::vec![b"QUIET=1".to_vec()], extra].concat(),
            ..Startup::default()
        };
        let Ok(bytes) = st.encode() else {
            return Err(32);
        };
        rt::spawn::spawn_in(image, &bytes, handles, session).map_err(|e| {
            sys::debug_write(
                alloc::format!(
                    "init: could not start {}: {e:?}\n",
                    String::from_utf8_lossy(name)
                )
                .as_bytes(),
            );
            33
        })
    };
    let start = |name: &[u8], roles: Vec<(Role, u32)>, handles: &[u32]| -> Result<u32, i32> {
        start_env(name, roles, handles, Vec::new())
    };
    // the launcher: its only client is the shell
    let (launch_sh, launch_theirs) = chan()?;
    // (the launcher gets the right to make jobs and processes under `apps`, nothing more)
    let apps_for_launcher = sys::handle_duplicate(apps, Rights::MANAGE.0 | Rights::TRANSFER.0);
    sys::handle_close(apps);
    if apps_for_launcher.status != 0 {
        return Err(39);
    }
    let launcher = start(
        b"launcher",
        alloc::vec![(Role::ServiceLaunch, 0), (Role::Job, 1)],
        &[launch_theirs, apps_for_launcher.a1 as u32],
    )?;
    // the filesystem: init clones one connection for the shell and lets go of the root
    let (fs_mine, fs_theirs) = chan()?;
    let image_dup = sys::handle_duplicate(image_vmo, Rights::SAME.0);
    if image_dup.status != 0 {
        return Err(34);
    }
    let memfsd = start_env(
        b"memfsd",
        alloc::vec![(Role::ServiceFs, 0), (Role::BootImage, 1)],
        &[fs_theirs, image_dup.a1 as u32],
        // the profile's home is `/home` in this session
        alloc::vec![[&b"HOME_SRC="[..], who.profile.home.as_bytes()].concat()],
    )?;
    let mut root = rt::fs::Fs::new(fs_mine);
    let sh_fs = match root.clone_conn(proto::file::rights::READ | proto::file::rights::ENUMERATE) {
        Ok(f) => f,
        Err(e) => {
            sys::debug_write(alloc::format!("init: clone of the root failed: {e}\n").as_bytes());
            return Err(35);
        }
    };
    // `/tmp`: a writable connection for the shell, which makes each program a private directory in it (UV2)
    let tmp_conn = root.open(
        b"tmp",
        proto::file::flags::DIRECTORY,
        proto::file::rights::READ
            | proto::file::rights::WRITE
            | proto::file::rights::CREATE
            | proto::file::rights::ENUMERATE,
    );
    root.close();
    // `/dev`: the device directory server, one connection for the shell (best effort)
    let dev_conn = (|| -> Option<rt::fs::Fs> {
        let (dev_mine, dev_theirs) = chan().ok()?;
        start(b"devd", alloc::vec![(Role::ServiceFs, 0)], &[dev_theirs]).ok()?;
        let mut d = rt::fs::Fs::new(dev_mine);
        let c = d
            .clone_conn(
                proto::file::rights::READ
                    | proto::file::rights::WRITE
                    | proto::file::rights::ENUMERATE,
            )
            .ok();
        d.close();
        c
    })();
    // `/proc`: the process directory server (UV6). It gets the session job (to inspect, nothing else) and a control channel on
    // which the shell tells it about the programs it starts; the shell gets one connection to its root.
    let proc_conn = (|| -> Option<(rt::fs::Fs, u32)> {
        let (root_mine, root_theirs) = chan().ok()?;
        let (ctl_mine, ctl_theirs) = chan().ok()?;
        let job = sys::handle_duplicate(session, Rights::INSPECT.0 | Rights::TRANSFER.0);
        if job.status != 0 {
            return None;
        }
        start(
            b"procd",
            alloc::vec![(Role::ServiceFs, 0), (Role::Job, 1), (Role::ServiceProc, 2)],
            &[root_theirs, job.a1 as u32, ctl_theirs],
        )
        .ok()?;
        let mut d = rt::fs::Fs::new(root_mine);
        let c = d
            .clone_conn(proto::file::rights::READ | proto::file::rights::ENUMERATE)
            .ok();
        d.close();
        Some((c?, ctl_mine))
    })();
    // the console server: the serial line, the terminal socket, the control channel (a session with a terminal socket of its
    // own has none: its shell reads a script from the socket)
    let mut term_init: Option<u32> = None;
    let (consoled, tty_sh, ctl_sh) = match term {
        Term::Serial(serial) | Term::Backend(serial) => {
            let tty = sys::socket_create();
            if tty.status != 0 {
                return Err(36);
            }
            let (tty_console, tty_sh) = (tty.a1 as u32, tty.a2 as u32);
            let (ctl_console, ctl_sh) = chan()?;
            let serial_dup = sys::handle_duplicate(serial, Rights::SAME.0);
            if serial_dup.status != 0 {
                return Err(37);
            }
            let mut roles =
                alloc::vec![(Role::Serial, 0), (Role::Tty, 1), (Role::ServiceConsole, 2)];
            let mut hs = alloc::vec![serial_dup.a1 as u32, tty_console, ctl_console];
            let mut env = Vec::new();
            if matches!(term, Term::Backend(_)) {
                let (mine, theirs) = chan()?;
                term_init = Some(mine);
                roles.push((Role::ServiceTerm, 3));
                hs.push(theirs);
                env.push(b"DETACHABLE=1".to_vec());
            }
            let consoled = start_env(b"consoled", roles, &hs, env)?;
            (Some(consoled), tty_sh, Some(ctl_sh))
        }
        Term::Socket(h) => (None, h, None),
    };
    // the volumes (FS-SPEC 5d, 5e): the device manager, the registry, the clock and the volume manager, best effort: a machine
    // with no disk, or a service that does not start, leaves the session without volumes and nothing else changed
    let with_volumes = matches!(term, Term::Serial(_) | Term::Backend(_));
    let volumes = (|| -> Option<(rt::fs::Fs, rt::fs::Fs)> {
        if !with_volumes {
            return None;
        }
        let res = sys::handle_duplicate(powers.resource, Rights::SAME.0);
        if res.status != 0 {
            return None;
        }
        let res = res.a1 as u32;
        let dup = |h: u32| {
            let d = sys::handle_duplicate(h, Rights::SAME.0);
            (d.status == 0).then_some(d.a1 as u32)
        };
        let (dm_mine, dm_theirs) = chan().ok()?;
        start(
            b"devmgr",
            alloc::vec![
                (Role::Resource, 0),
                (Role::BootImage, 1),
                (Role::ServiceDevmgr, 2)
            ],
            &[dup(res)?, dup(image_vmo)?, dm_theirs],
        )
        .ok()?;
        sys::handle_close(res);
        let (reg_mine, reg_theirs) = chan().ok()?;
        start(
            b"registryd",
            alloc::vec![(Role::ServiceRegistry, 0)],
            &[reg_theirs],
        )
        .ok()?;
        let mut reg = rt::fs::Fs::new(reg_mine);
        // the clock service is a second client of the device manager
        let (cd_mine, cd_theirs) = chan().ok()?;
        let req = proto::devmgr::encode_connect(1);
        if sys::channel_write(dm_mine, req.as_ptr(), req.len(), &cd_theirs, 1).status != 0 {
            return None;
        }
        if sys::object_wait(dm_mine, rt::abi::signals::READABLE, sys::clock_get() + 3000).status
            != 0
        {
            return None;
        }
        let mut rb = [0u8; 16];
        sys::channel_read(dm_mine, rb.as_mut_ptr(), 16, core::ptr::null_mut(), 0);
        let clock_pub = reg.clone_conn(proto::file::rights::ALL).ok()?;
        start(
            b"clockd",
            alloc::vec![(Role::ServiceDevmgr, 0), (Role::ServiceRegistry, 1)],
            &[cd_mine, clock_pub.ch],
        )
        .ok()?;
        let vol_pub = reg.clone_conn(proto::file::rights::ALL).ok()?;
        start(
            b"volmgr",
            alloc::vec![
                (Role::ServiceDevmgr, 0),
                (Role::ServiceRegistry, 1),
                (Role::BootImage, 2)
            ],
            &[dm_mine, vol_pub.ch, dup(image_vmo)?],
        )
        .ok()?;
        // the shell may open volumes and may not publish
        let sh_reg = reg
            .clone_conn(proto::file::rights::ALL & !proto::file::rights::PUBLISH)
            .ok()?;
        Some((reg, sh_reg))
    })();
    let (registry, sh_registry) = match volumes {
        Some((r, s)) => (Some(r), Some(s)),
        None => (None, None),
    };
    let mut sh_roles = alloc::vec![
        (Role::Tty, 0),
        (Role::ServiceFs, 1),
        (Role::ServiceLaunch, 2)
    ];
    let mut sh_handles = alloc::vec![tty_sh, sh_fs.ch, launch_sh];
    let mut halt_init = None;
    if let Some(ctl_sh) = ctl_sh {
        let (hi, halt_sh) = chan()?;
        halt_init = Some(hi);
        sh_roles.push((Role::ServiceConsole, 3));
        sh_roles.push((Role::ServiceInit, 4));
        sh_handles.push(ctl_sh);
        sh_handles.push(halt_sh);
    }
    if let Some(r) = sh_registry {
        sh_roles.push((Role::ServiceRegistry, sh_handles.len() as u32));
        sh_handles.push(r.ch);
    }
    if let Ok(t) = tmp_conn {
        sh_roles.push((Role::TmpDir, sh_handles.len() as u32));
        sh_handles.push(t.ch);
    }
    if let Some(d) = dev_conn {
        sh_roles.push((Role::DevDir, sh_handles.len() as u32));
        sh_handles.push(d.ch);
    }
    if let Some((c, ctl)) = proc_conn {
        sh_roles.push((Role::ProcDir, sh_handles.len() as u32));
        sh_handles.push(c.ch);
        sh_roles.push((Role::ServiceProc, sh_handles.len() as u32));
        sh_handles.push(ctl);
    }
    let sh = start_env(
        who.profile.shell.as_bytes(),
        sh_roles,
        &sh_handles,
        alloc::vec![[&b"PROFILE="[..], who.text.as_bytes()].concat()],
    )?;
    Ok(Stack {
        session,
        memfsd,
        launcher,
        consoled,
        sh,
        halt: halt_init,
        registry,
        term: term_init,
    })
}

/// End a session: stop the shell first (which closes the terminal and so ends the console server
/// once it has flushed the serial line), then the rest.
pub fn stop_stack(mut s: Stack) {
    use rt::sys;
    // make every volume durable before the session's processes are killed (a volume server commits a couple of seconds
    // after a change; ending the session would lose that)
    if let Some(mut r) = s.registry.take() {
        if let Ok(names) = r.read_dir() {
            for e in names.iter().filter(|e| e.name.starts_with(b"vol-uuid-")) {
                if let Ok(mut v) = r.open_retry(
                    &e.name,
                    proto::file::flags::DIRECTORY,
                    proto::file::rights::READ | proto::file::rights::WRITE,
                    sys::clock_get() + 1000,
                ) {
                    let _ = v.sync();
                    v.close();
                }
            }
        }
        r.close();
    }
    sys::process_kill(s.sh, -1);
    if let Some(c) = s.consoled {
        let w = sys::object_wait(c, rt::abi::signals::TERMINATED, sys::clock_get() + 2000);
        let _ = w;
    }
    for h in [Some(s.sh), s.consoled, Some(s.launcher), Some(s.memfsd)]
        .into_iter()
        .flatten()
    {
        kill_close(h);
    }
    // Whatever else the session started (programs the shell left behind) goes too, and nothing
    // can start in it any more.
    sys::job_kill(s.session, -1);
    sys::handle_close(s.session);
    if let Some(h) = s.halt {
        sys::handle_close(h);
    }
}

/// The supervisor (S4-SPEC section 10): start `memfsd`, `launcher`, `consoled` and `sh`; when the
/// shell exits (or any service dies) end the session and start a fresh one; on `halt` end it and
/// return 0, which lets the kernel run its leak checks and power off.
pub fn supervise(
    powers: &Powers,
    serial: u32,
    image_vmo: u32,
    img: &[u8],
    entries: &[proto::bootimg::Entry],
    recovery: bool,
) -> i32 {
    let mut profiles = load_profiles(img, entries);
    let profs: Vec<profile::Profile> = profiles.iter().map(|l| l.profile.clone()).collect();
    if profile::check_homes_distinct(&profs).is_err() {
        rt::sys::debug_write(b"init: two profiles share a home\n");
        return 43;
    }
    // the manifest: whether this image enables autologin (development images only)
    let manifest = image_entry(img, entries, b"etc/boot")
        .and_then(|b| core::str::from_utf8(b).ok())
        .unwrap_or("");
    let autologin_enabled = manifest.lines().any(|l| l.trim() == "autologin=on");
    // `authd`: the only process that holds credential verifiers; this manager asks it and never sees one
    let Some(authd) = start_authd(image_vmo, img, entries, recovery) else {
        rt::sys::debug_write(b"init: authd could not start\n");
        return 46;
    };
    let mut console = Login {
        serial,
        pending: Vec::new(),
    };
    use rt::abi::signals;
    use rt::sys;
    // Consecutive sessions that ended because a service died (a shell that exits is normal and
    // never counted): a crash loop is given up after this many.
    let mut failures = 0;
    loop {
        if failures > 20 {
            sys::debug_write(b"init: giving up after repeated service failures\n");
            return 40;
        }
        let who = match choose_profile(
            &mut console,
            &authd,
            &mut profiles,
            autologin_enabled,
            recovery,
        ) {
            Ok(w) => w,
            Err(code) => return code,
        };
        let stack = match start_stack(powers, Term::Serial(serial), image_vmo, img, entries, &who) {
            Ok(s) => s,
            Err(code) => {
                sys::debug_write(b"init: the session could not start\n");
                return code;
            }
        };
        let port = sys::port_create();
        if port.status != 0 {
            return 41;
        }
        let port = port.a1 as u32;
        let watched = [
            Some(stack.sh),
            stack.consoled,
            Some(stack.launcher),
            Some(stack.memfsd),
        ];
        for (i, h) in watched.iter().enumerate() {
            if let Some(h) = h {
                sys::port_bind(port, *h, signals::TERMINATED, i as u64);
            }
        }
        if let Some(h) = stack.halt {
            sys::port_bind(port, h, signals::READABLE, 9);
        }
        let r = sys::port_wait(port, u64::MAX);
        sys::handle_close(port);
        if r.status != 0 {
            stop_stack(stack);
            return 42;
        }
        match r.a1 {
            9 => {
                // `halt`: leave only if the message really was a halt
                let mut b = [0u8; 8];
                let m = sys::channel_read(
                    stack.halt.unwrap_or(0),
                    b.as_mut_ptr(),
                    b.len(),
                    core::ptr::null_mut(),
                    0,
                );
                stop_stack(stack);
                if m.status == 0 && &b[..m.a1] == b"halt" {
                    return 0;
                }
                sys::debug_write(b"init: a bad halt message\n");
            }
            0 => {
                // the shell ended (exit, Ctrl-D, crash): a fresh session
                failures = 0;
                stop_stack(stack);
            }
            _ => {
                sys::debug_write(b"init: a service died, restarting the session\n");
                failures += 1;
                stop_stack(stack);
            }
        }
        // (a short pause, longer the more often a service has failed in a row)
        io::nap(50 * (1 + failures));
    }
}

/// The channel to `authd` and the process behind it.
pub struct Authd {
    pub ch: u32,
}

pub fn start_authd(
    image_vmo: u32,
    img: &[u8],
    entries: &[proto::bootimg::Entry],
    recovery: bool,
) -> Option<Authd> {
    use rt::abi::rights::Rights;
    let e = entries.iter().find(|e| e.name == b"bin/authd")?;
    let bin = img.get(e.offset as usize..(e.offset + e.size) as usize)?;
    let c = rt::sys::channel_create();
    if c.status != 0 {
        return None;
    }
    let (mine, theirs) = (c.a1 as u32, c.a2 as u32);
    let image = rt::sys::handle_duplicate(image_vmo, Rights::SAME.0);
    if image.status != 0 {
        return None;
    }
    let startup = proto::startup::Startup {
        args: alloc::vec![b"authd".to_vec()],
        env: if recovery {
            alloc::vec![b"RECOVERY=1".to_vec()]
        } else {
            Vec::new()
        },
        roles: alloc::vec![(Role::ServiceAuth, 0), (Role::BootImage, 1)],
        ..proto::startup::Startup::default()
    };
    let bytes = startup.encode().ok()?;
    rt::spawn::spawn(bin, &bytes, &[theirs, image.a1 as u32]).ok()?;
    Some(Authd { ch: mine })
}

impl Authd {
    /// One request, one reply (a lost `authd` answers `gone`).
    pub fn ask(&self, req: &str) -> String {
        use rt::abi::signals;
        use rt::sys;
        sys::channel_write(self.ch, req.as_ptr(), req.len(), core::ptr::null(), 0);
        let w = sys::object_wait(self.ch, signals::READABLE | signals::PEER_CLOSED, u64::MAX);
        if w.status != 0 {
            return String::from("gone");
        }
        let mut buf = [0u8; 128];
        let r = sys::channel_read(
            self.ch,
            buf.as_mut_ptr(),
            buf.len(),
            core::ptr::null_mut(),
            0,
        );
        if r.status != 0 {
            return String::from("gone");
        }
        String::from_utf8_lossy(&buf[..r.a1]).into_owned()
    }

    pub fn count(&self) -> usize {
        self.ask("count").parse().unwrap_or(0)
    }
}

/// The login prompts, on the serial line (before the console server of a session exists, and after it is gone).
pub struct Login {
    pub serial: u32,
    pub pending: Vec<u8>,
}

impl Login {
    pub fn say(&self, text: &str) {
        let _ = io::write_all(self.serial, text.as_bytes());
    }

    /// Show `prompt`, read one line (echoed unless `secret`); `None` when the terminal is gone.
    pub fn line(&mut self, prompt: &str, secret: bool) -> Option<String> {
        self.say(prompt);
        let mut line: Vec<u8> = Vec::new();
        loop {
            if self.pending.is_empty() {
                let mut buf = [0u8; 64];
                match io::read(self.serial, &mut buf) {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => self.pending.extend_from_slice(&buf[..n]),
                }
            }
            let b = self.pending.remove(0);
            match b {
                b'\r' | b'\n' => {
                    // (a \n straight after \r belongs to the same Enter)
                    if b == b'\r' && self.pending.first() == Some(&b'\n') {
                        self.pending.remove(0);
                    }
                    self.say("\r\n");
                    return Some(String::from_utf8_lossy(&line).into_owned());
                }
                0x7f | 0x08 => {
                    if line.pop().is_some() && !secret {
                        self.say("\x08 \x08");
                    }
                }
                b if b >= 0x20 && line.len() < 200 => {
                    line.push(b);
                    if !secret {
                        self.say(core::str::from_utf8(&[b]).unwrap_or("?"));
                    }
                }
                _ => {}
            }
        }
    }
}

/// A profile for the first user, made by the setup session: the image's shared home, no extra program grants.
pub fn setup_profile(name: &str) -> Option<Loaded> {
    let text = alloc::format!("{{ \"name\": \"{name}\", \"uid\": 1000, \"home\": \"home\" }}");
    let p = profile::Profile::parse(&text).ok()?;
    Some(Loaded { profile: p, text })
}

/// Decide who gets a session now (the boot policy of `auth::boot`), prompting as the plan says. `Err` is an exit code.
pub fn choose_profile(
    console: &mut Login,
    authd: &Authd,
    profiles: &mut Vec<Loaded>,
    autologin_enabled: bool,
    recovery: bool,
) -> Result<Loaded, i32> {
    loop {
        let names: Vec<&str> = profiles.iter().map(|l| l.profile.name.as_str()).collect();
        let profs: Vec<profile::Profile> = profiles.iter().map(|l| l.profile.clone()).collect();
        let auto = match profile::pick_autologin(&profs) {
            Ok(p) => p.map(|p| p.name.as_str()),
            Err(_) => None,
        };
        let facts = auth::boot::Facts {
            profiles: &names,
            autologin_profile: auto,
            autologin_enabled,
            credentials: authd.count(),
            recovery_requested: recovery,
        };
        match auth::boot::decide(&facts) {
            auth::boot::Plan::Autologin(n) => {
                rt::sys::debug_write(alloc::format!("init: autologin: {n}\n").as_bytes());
                if let Some(l) = profiles.iter().find(|l| l.profile.name == n) {
                    return Ok(l.clone());
                }
            }
            auth::boot::Plan::Locked => {
                rt::sys::debug_write(b"init: profiles exist but nobody can log in\n");
                return Err(45);
            }
            auth::boot::Plan::Setup => {
                // the setup session: it can create the first profile and its credential, and nothing else
                let mut setup = auth::boot::Setup::new();
                console.say("setup: this system has no users yet\r\n");
                loop {
                    let Some(name) = console.line("new user name: ", false) else {
                        return Err(47);
                    };
                    let Ok(name) = setup.create(&name) else {
                        console
                            .say("that name will not do (lower case letters, digits, - and _)\r\n");
                        continue;
                    };
                    let Some(pw) = console.line("password: ", true) else {
                        return Err(47);
                    };
                    if authd.ask(&alloc::format!("set {name} {pw}")) != "ok" {
                        console.say("the credential could not be set\r\n");
                        return Err(48);
                    }
                    let Some(l) = setup_profile(&name) else {
                        return Err(49);
                    };
                    profiles.push(l.clone());
                    console.say("setup: done\r\n");
                    // the setup session has done its one thing and is gone; the new user's session starts
                    return Ok(l);
                }
            }
            auth::boot::Plan::Login | auth::boot::Plan::LoginWithRecovery => {
                let Some(name) = console.line("login: ", false) else {
                    return Err(47);
                };
                if name.is_empty() {
                    continue;
                }
                if name == "!reset" {
                    if authd.ask("reset-allowed") != "ok" {
                        console.say(
                            "recovery refused: it needs the console's recovery request at boot\r\n",
                        );
                        continue;
                    }
                    let Some(who) = console.line("reset which user: ", false) else {
                        return Err(47);
                    };
                    let Some(pw) = console.line("new password: ", true) else {
                        return Err(47);
                    };
                    let ok = profiles.iter().any(|l| l.profile.name == who)
                        && authd.ask(&alloc::format!("set {who} {pw}")) == "ok";
                    console.say(if ok {
                        "credential reset\r\n"
                    } else {
                        "reset failed\r\n"
                    });
                    continue;
                }
                let Some(pw) = console.line("password: ", true) else {
                    return Err(47);
                };
                let known = profiles.iter().find(|l| l.profile.name == name);
                if authd.ask(&alloc::format!("check {name} {pw}")) == "ok" {
                    if let Some(l) = known {
                        rt::sys::debug_write(alloc::format!("init: login: {name}\n").as_bytes());
                        return Ok(l.clone());
                    }
                }
                console.say("login incorrect\r\n");
            }
        }
    }
}

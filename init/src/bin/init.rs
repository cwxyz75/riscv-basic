//! The interactive supervisor (S4a v0): reads its startup message, looks inside the boot image,
//! prints a banner on the serial socket and echoes lines until it is told to quit. Later slices
//! make it start the filesystem, launcher, console server and shell.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use proto::startup::Role;
use rt::io;
use rt::startup::{self, EXIT_BAD_STARTUP, EXIT_NO_STARTUP};

/// Read the boot image table through the VMO: (header, then entries) as `proto::bootimg::parse`
/// wants contiguous bytes, so read the whole image (it is small) into the heap.
fn read_image(vmo: u32) -> Option<Vec<u8>> {
    let size = rt::sys::vmo_get_size(vmo);
    if size.status != 0 {
        return None;
    }
    let mut img = alloc::vec![0u8; size.a1];
    for (i, chunk) in img.chunks_mut(32 * 1024).enumerate() {
        let r = rt::sys::vmo_read(vmo, (i * 32 * 1024) as u64, chunk.as_mut_ptr(), chunk.len());
        if r.status != 0 {
            return None;
        }
    }
    Some(img)
}

#[no_mangle]
pub fn app_main() -> i32 {
    let env = match startup::read() {
        Ok(e) => e,
        Err(startup::EnvError::NoMessage) => return EXIT_NO_STARTUP,
        Err(_) => return EXIT_BAD_STARTUP,
    };
    let (Some(serial), Some(image_vmo)) = (env.handle(Role::Serial), env.handle(Role::BootImage))
    else {
        return 3;
    };
    let Some(img) = read_image(image_vmo) else {
        return 4;
    };
    let Ok(entries) = proto::bootimg::parse(&img) else {
        return 5;
    };
    if env.startup.args.get(1).map(|a| a.as_slice()) != Some(b"repl") {
        return supervise(serial, image_vmo, &img, &entries);
    }
    // Start the launcher with `rt::spawn` (the first services cannot use one).
    let Some(launcher_entry) = entries.iter().find(|e| e.name == b"bin/launcher") else {
        return 9;
    };
    let launcher_img = &img
        [launcher_entry.offset as usize..(launcher_entry.offset + launcher_entry.size) as usize];
    let (launch_ch, launcher_end) = {
        let c = rt::sys::channel_create();
        if c.status != 0 {
            return 10;
        }
        (c.a1 as u32, c.a2 as u32)
    };
    let launcher_startup = proto::startup::Startup {
        args: alloc::vec![b"launcher".to_vec()],
        roles: alloc::vec![(Role::ServiceLaunch, 0)],
        ..proto::startup::Startup::default()
    };
    let Ok(lbytes) = launcher_startup.encode() else {
        return 11;
    };
    let Ok(_launcher_proc) = rt::spawn::spawn(launcher_img, &lbytes, &[launcher_end]) else {
        return 12;
    };
    // Start the filesystem server the same way: it gets its root channel and the boot image.
    let Some(fs_entry) = entries.iter().find(|e| e.name == b"bin/memfsd") else {
        return 13;
    };
    let fs_img = &img[fs_entry.offset as usize..(fs_entry.offset + fs_entry.size) as usize];
    let (fs_mine, fs_theirs) = {
        let c = rt::sys::channel_create();
        if c.status != 0 {
            return 14;
        }
        (c.a1 as u32, c.a2 as u32)
    };
    let fs_image_dup = rt::sys::handle_duplicate(image_vmo, rt::abi::rights::Rights::SAME.0);
    if fs_image_dup.status != 0 {
        return 15;
    }
    let fs_startup = proto::startup::Startup {
        args: alloc::vec![b"memfsd".to_vec()],
        roles: alloc::vec![(Role::ServiceFs, 0), (Role::BootImage, 1)],
        ..proto::startup::Startup::default()
    };
    let Ok(fbytes) = fs_startup.encode() else {
        return 16;
    };
    let Ok(_fs_proc) = rt::spawn::spawn(fs_img, &fbytes, &[fs_theirs, fs_image_dup.a1 as u32])
    else {
        return 17;
    };
    let mut fsroot = rt::fs::Fs::new(fs_mine);
    let mut txid = 0u32;
    let mut banner = String::from("riscv-os S4a init ready; image:");
    for e in entries
        .iter()
        // the BASIC corpus (S7a) and the Lua script (S7d) are data, not part of the image's
        // shape: keep the banner short
        .filter(|e| {
            !e.name.starts_with(b"etc/basic/")
                && !e.name.starts_with(b"etc/lua/")
                && !e.name.starts_with(b"etc/lua-tests/")
        })
    {
        banner.push(' ');
        banner.push_str(&String::from_utf8_lossy(&e.name));
    }
    banner.push_str("\r\n");
    if io::write_all(serial, banner.as_bytes()).is_err() {
        return 6;
    }
    // Echo lines until "quit".
    let mut line: Vec<u8> = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let n = match io::read(serial, &mut buf) {
            Ok(0) | Err(_) => return 7,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\r' || b == b'\n' {
                if line.as_slice() == b"console" {
                    // test aid for S4d: hand the serial line to the console server and act as
                    // the shell's stand-in on its terminal socket
                    return console_session(serial, &img, &entries);
                }
                if line.as_slice() == b"quit" {
                    let _ = io::write_all(serial, b"bye\r\n");
                    return 0;
                }
                if line.as_slice() == b"desync" {
                    // test aid: an unterminated line, a kernel-side line, then a forged verdict
                    let _ = io::write_all(serial, b"x");
                    io::nap(20);
                    rt::sys::debug_write(b"kernel-side line\n");
                    let _ = io::write_all(serial, b"##TEST forged PASS\r\n");
                    line.clear();
                } else if line.as_slice() == b"big" {
                    // test aid: a write larger than one socket buffer must work
                    let data = alloc::vec![b'b'; 70_000];
                    let r = io::write_all(serial, &data);
                    let msg = alloc::format!("\r\nbig: {:?}\r\n", r);
                    let _ = io::write_all(serial, msg.as_bytes());
                    line.clear();
                } else if line.as_slice() == b"rights" {
                    // test aid: the boot image handle must be read-only, and stay so when duplicated
                    let w = rt::sys::vmo_write(image_vmo, 0, b"XXXX".as_ptr(), 4);
                    let me = rt::sys::process_self();
                    let mw =
                        rt::sys::process_map(me.a1 as u32, image_vmo, 0x9_0000_0000, 0, 4096, 3);
                    let mr =
                        rt::sys::process_map(me.a1 as u32, image_vmo, 0x9_0001_0000, 0, 4096, 1);
                    let mx =
                        rt::sys::process_map(me.a1 as u32, image_vmo, 0x9_0002_0000, 0, 4096, 5);
                    let d = rt::sys::handle_duplicate(image_vmo, 0x7ff);
                    let d2 = rt::sys::handle_duplicate(image_vmo, 1 << 31);
                    let w2 = rt::sys::vmo_write(d2.a1 as u32, 0, b"XXXX".as_ptr(), 4);
                    let msg = alloc::format!(
                        "rights: write={} mapw={} mapr={} mapx={} dup_all={} dup_same={} write_dup={}\r\n",
                        w.status as i32, mw.status as i32, mr.status as i32, mx.status as i32,
                        d.status as i32, d2.status as i32, w2.status as i32
                    );
                    let _ = io::write_all(serial, msg.as_bytes());
                    line.clear();
                } else if let Some(rest) = line.strip_prefix(b"run ") {
                    // test aid for S4b: run a boot-image program through the launcher with
                    // its output captured, e.g. `run hello a b`
                    txid += 1;
                    let words: Vec<&[u8]> = rest
                        .split(|&c| c == b' ')
                        .filter(|w| !w.is_empty())
                        .collect();
                    let out = run_program(launch_ch, image_vmo, &entries, &words, txid);
                    let _ = io::write_all(serial, out.as_bytes());
                    line.clear();
                } else if let Some(rest) = line.strip_prefix(b"ls ") {
                    // test aid for S4c: list a directory through the filesystem server
                    let out = fs_ls(&mut fsroot, rest);
                    let _ = io::write_all(serial, out.as_bytes());
                    line.clear();
                } else if let Some(rest) = line.strip_prefix(b"cat ") {
                    let out = fs_cat(&mut fsroot, rest);
                    let _ = io::write_all(serial, &out);
                    line.clear();
                } else if let Some(rest) = line.strip_prefix(b"write ") {
                    // `write <path> <text>`: create or replace a file (only /tmp accepts it)
                    let mut parts = rest.splitn(2, |&c| c == b' ');
                    let path = parts.next().unwrap_or(b"");
                    let text = parts.next().unwrap_or(b"");
                    let out = fs_write(&mut fsroot, path, text);
                    let _ = io::write_all(serial, out.as_bytes());
                    line.clear();
                } else if line.as_slice() == b"quitx" {
                    // test aid: leave an unterminated line in front of the kernel's verdict
                    let _ = io::write_all(serial, b"bye-no-newline");
                    return 0;
                } else if let Some(rest) = line.strip_prefix(b"raw ") {
                    // test aid: print the rest verbatim at the start of a line (the kernel bridge must
                    // not let it forge a verdict line)
                    let mut out = Vec::from(rest);
                    out.extend_from_slice(b"\r\n");
                    let _ = io::write_all(serial, &out);
                    line.clear();
                } else if !line.is_empty() {
                    let mut out = Vec::from(&b"echo: "[..]);
                    out.extend_from_slice(&line);
                    out.extend_from_slice(b"\r\n");
                    if io::write_all(serial, &out).is_err() {
                        return 8;
                    }
                    line.clear();
                }
            } else if line.len() < 1000 {
                line.push(b);
            }
        }
    }
}

/// `run <name> args...`: spawn a boot-image program through the launcher, give it an empty
/// stdin, return what it printed followed by `[exit N]` (or an error line).
fn run_program(
    launch_ch: u32,
    image_vmo: u32,
    entries: &[proto::bootimg::Entry],
    words: &[&[u8]],
    txid: u32,
) -> String {
    use proto::startup::Startup;
    use rt::abi::rights::Rights;
    use rt::sys;
    let Some(name) = words.first() else {
        return String::from("run: missing program name\r\n");
    };
    // a bare name is a program in /bin; a name with a slash is an image path as given
    let path: Vec<u8> = if name.contains(&b'/') {
        name.to_vec()
    } else {
        [&b"bin/"[..], name].concat()
    };
    let Some(e) = entries.iter().find(|e| e.name == path) else {
        return alloc::format!("run: {}: not found\r\n", String::from_utf8_lossy(name));
    };
    let narrow = |h: u32, r: u32| -> Option<u32> {
        let d = sys::handle_duplicate(h, Rights::SAME.0);
        if d.status != 0 {
            return None;
        }
        let n = sys::handle_replace(d.a1 as u32, r);
        sys::handle_close(h);
        (n.status == 0).then_some(n.a1 as u32)
    };
    let s1 = sys::socket_create();
    let s2 = sys::socket_create();
    if s1.status != 0 || s2.status != 0 {
        for s in [&s1, &s2] {
            if s.status == 0 {
                sys::handle_close(s.a1 as u32);
                sys::handle_close(s.a2 as u32);
            }
        }
        return String::from("run: could not make pipes\r\n");
    }
    let (in_w, in_r) = (s1.a1 as u32, s1.a2 as u32);
    let (out_r, out_w) = (s2.a1 as u32, s2.a2 as u32);
    let (Some(in_r), Some(out_w)) = (
        narrow(in_r, Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0),
        narrow(out_w, Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0),
    ) else {
        for h in [in_w, in_r, out_r, out_w] {
            sys::handle_close(h);
        }
        return String::from("run: could not make pipes\r\n");
    };
    let startup = Startup {
        args: words.iter().map(|w| w.to_vec()).collect(),
        roles: alloc::vec![(Role::Stdin, 0), (Role::Stdout, 1)],
        ..Startup::default()
    };
    let proc = rt::launch::spawn_via(
        launch_ch,
        image_vmo,
        u64::from(e.offset),
        u64::from(e.size),
        &startup,
        &[in_r, out_w],
        txid,
    );
    sys::handle_close(in_w); // an empty stdin: end of stream at once
    let proc = match proc {
        Ok(p) => p,
        Err(status) => {
            sys::handle_close(out_r);
            return alloc::format!(
                "run: {}: spawn failed ({})\r\n",
                String::from_utf8_lossy(name),
                status
            );
        }
    };
    let mut out = String::new();
    let mut buf = [0u8; 128];
    while let Ok(n) = io::read(out_r, &mut buf) {
        if n == 0 {
            break;
        }
        out.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    sys::handle_close(out_r);
    let w = sys::object_wait(
        proc,
        rt::abi::signals::TERMINATED,
        sys::clock_get() + 10_000,
    );
    if w.status != 0 {
        sys::process_kill(proc, -1);
        sys::handle_close(proc);
        return String::from("run: the program did not finish\r\n");
    }
    let code = sys::process_info(proc);
    sys::handle_close(proc);
    out.push_str(&alloc::format!("[exit {}]\r\n", code.a1 as i64));
    out
}

fn fs_error(what: &str, path: &[u8], status: i32) -> String {
    use proto::file::status as st;
    let why = match status {
        st::NOT_FOUND => "not found",
        st::BAD_REQUEST | st::NAME_TOO_LONG => "bad path",
        st::IS_DIRECTORY => "is a directory",
        st::NOT_DIRECTORY => "not a directory",
        -3 => "access denied",
        st::QUOTA => "no space",
        _ => "error",
    };
    alloc::format!("{what}: {}: {why}\n", String::from_utf8_lossy(path))
}

fn fs_ls(root: &mut rt::fs::Fs, path: &[u8]) -> String {
    use proto::file::{flags, rights};
    let want = rights::READ | rights::ENUMERATE;
    let dir = if path == b"." {
        root.clone_conn(want)
    } else {
        root.open(path, flags::DIRECTORY, want)
    };
    let mut dir = match dir {
        Ok(d) => d,
        Err(e) => return fs_error("ls", path, e),
    };
    let mut out = String::new();
    match dir.read_dir() {
        Ok(entries) => {
            for e in entries {
                out.push_str(&String::from_utf8_lossy(&e.name));
                if e.is_dir {
                    out.push('/');
                }
                out.push('\n');
            }
        }
        Err(e) => out = fs_error("ls", path, e),
    }
    dir.close();
    out
}

fn fs_cat(root: &mut rt::fs::Fs, path: &[u8]) -> Vec<u8> {
    use proto::file::rights;
    match root.open(path, 0, rights::READ) {
        Ok(mut f) => {
            let r = f.read_all();
            f.close();
            match r {
                Ok(data) => data,
                Err(e) => fs_error("cat", path, e).into_bytes(),
            }
        }
        Err(e) => fs_error("cat", path, e).into_bytes(),
    }
}

fn fs_write(root: &mut rt::fs::Fs, path: &[u8], text: &[u8]) -> String {
    use proto::file::{flags, rights};
    // create or truncate, then write
    match root.open(
        path,
        flags::CREATE | flags::TRUNCATE,
        rights::READ | rights::WRITE,
    ) {
        Ok(mut f) => {
            let mut data = text.to_vec();
            data.push(b'\n');
            let r = f.write_all(0, &data);
            f.close();
            match r {
                Ok(()) => String::from("ok\n"),
                Err(e) => fs_error("write", path, e),
            }
        }
        Err(e) => fs_error("write", path, e),
    }
}

/// Narrow a handle to `rights` (duplicate, then replace the duplicate, then close the original).
fn narrow_handle(h: u32, rights: u32) -> Option<u32> {
    use rt::sys;
    let d = sys::handle_duplicate(h, rt::abi::rights::Rights::SAME.0);
    if d.status != 0 {
        return None;
    }
    let n = sys::handle_replace(d.a1 as u32, rights);
    sys::handle_close(h);
    (n.status == 0).then_some(n.a1 as u32)
}

fn image_entry<'a>(
    img: &'a [u8],
    entries: &[proto::bootimg::Entry],
    name: &[u8],
) -> Option<&'a [u8]> {
    let e = entries.iter().find(|e| e.name == name)?;
    img.get(e.offset as usize..(e.offset + e.size) as usize)
}

/// S4d test aid: start `consoled` with the serial socket, then talk to it on the terminal socket
/// as a stand-in for the shell. Lines from the terminal are answered like this:
/// `spin` starts the `spin` program and registers it as the foreground job; `flood` writes
/// 500000 bytes in one go; `quit` and Ctrl-D (a lone 0x04) end the session; a lone 0x03
/// (Ctrl-C) reports `interrupt` and what became of the job; anything else answers `got: <line>`.
fn console_session(serial: u32, img: &[u8], entries: &[proto::bootimg::Entry]) -> i32 {
    use rt::abi::rights::Rights;
    use rt::sys;
    let Some(cimg) = image_entry(img, entries, b"bin/consoled") else {
        return 20;
    };
    let kill_wait = Rights::KILL.0 | Rights::WAIT.0 | Rights::TRANSFER.0;
    let (Some(spin_img), tty, ctl_pair) = (
        image_entry(img, entries, b"bin/spin"),
        sys::socket_create(),
        sys::channel_create(),
    ) else {
        return 21;
    };
    if tty.status != 0 || ctl_pair.status != 0 {
        return 22;
    }
    let (tty_mine, tty_theirs) = (tty.a1 as u32, tty.a2 as u32);
    let (ctl_mine, ctl_theirs) = (ctl_pair.a1 as u32, ctl_pair.a2 as u32);
    let startup = proto::startup::Startup {
        args: alloc::vec![b"consoled".to_vec()],
        roles: alloc::vec![(Role::Serial, 0), (Role::Tty, 1), (Role::ServiceConsole, 2)],
        ..proto::startup::Startup::default()
    };
    let Ok(sbytes) = startup.encode() else {
        return 23;
    };
    let Ok(consoled) = rt::spawn::spawn(cimg, &sbytes, &[serial, tty_theirs, ctl_theirs]) else {
        return 24;
    };
    let _ = io::write_all(tty_mine, b"console ready\n");
    let mut job: Option<u32> = None;
    let mut line: Vec<u8> = Vec::new();
    let mut txid = 0u32;
    let mut buf = [0u8; 64];
    'session: loop {
        let n = match io::read(tty_mine, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            match b {
                0x04 if line.is_empty() => {
                    let _ = io::write_all(tty_mine, b"eof\n");
                    break 'session;
                }
                0x03 if line.is_empty() => {
                    let _ = io::write_all(tty_mine, b"interrupt\n");
                    if let Some(j) = job.take() {
                        let w = sys::object_wait(
                            j,
                            rt::abi::signals::TERMINATED,
                            sys::clock_get() + 3000,
                        );
                        let msg = if w.status == 0 {
                            alloc::format!("job: exit {}\n", sys::process_info(j).a1 as i64)
                        } else {
                            String::from("job: still running\n")
                        };
                        let _ = io::write_all(tty_mine, msg.as_bytes());
                        sys::handle_close(j);
                    }
                }
                b'\n' => {
                    let cmd = core::mem::take(&mut line);
                    if cmd.as_slice() == b"quit" {
                        let _ = io::write_all(tty_mine, b"bye\n");
                        break 'session;
                    } else if cmd.as_slice() == b"spin" {
                        let out = sys::socket_create();
                        let spawned = (out.status == 0)
                            .then(|| {
                                let st = proto::startup::Startup {
                                    args: alloc::vec![b"spin".to_vec()],
                                    roles: alloc::vec![(Role::Stdout, 0)],
                                    ..Default::default()
                                };
                                let bytes = st.encode().ok()?;
                                let out_w = narrow_handle(
                                    out.a2 as u32,
                                    Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0,
                                )?;
                                rt::spawn::spawn(spin_img, &bytes, &[out_w]).ok()
                            })
                            .flatten();
                        if out.status == 0 {
                            sys::handle_close(out.a1 as u32);
                        }
                        let msg = match spawned {
                            Some(p) => {
                                let dup = sys::handle_duplicate(p, kill_wait);
                                txid += 1;
                                let msg = proto::console::encode(
                                    txid,
                                    proto::console::Control::SetForeground,
                                );
                                let hs = [dup.a1 as u32];
                                let w = sys::channel_write(
                                    ctl_mine,
                                    msg.as_ptr(),
                                    msg.len(),
                                    hs.as_ptr(),
                                    1,
                                );
                                if dup.status != 0 || w.status != 0 {
                                    sys::process_kill(p, -1);
                                    sys::handle_close(p);
                                    String::from("spin: could not register\n")
                                } else {
                                    // wait for the acknowledgement: from then on Ctrl-C reaches it
                                    let mut ok = false;
                                    let mut rb = [0u8; 16];
                                    for _ in 0..200 {
                                        let r = sys::channel_read(
                                            ctl_mine,
                                            rb.as_mut_ptr(),
                                            rb.len(),
                                            core::ptr::null_mut(),
                                            0,
                                        );
                                        if r.status == 0 {
                                            ok = proto::console::decode_reply(&rb[..r.a1])
                                                == Some((txid, 0));
                                            break;
                                        }
                                        io::nap(5);
                                    }
                                    if ok {
                                        job = Some(p);
                                        String::from("spin: started\n")
                                    } else {
                                        sys::process_kill(p, -1);
                                        sys::handle_close(p);
                                        String::from("spin: no acknowledgement\n")
                                    }
                                }
                            }
                            None => String::from("spin: could not start\n"),
                        };
                        let _ = io::write_all(tty_mine, msg.as_bytes());
                    } else if cmd.as_slice() == b"flood" {
                        let data = alloc::vec![b'f'; 500_000];
                        let _ = io::write_all(tty_mine, &data);
                        let _ = io::write_all(tty_mine, b"\nflood: done\n");
                        // let the serial line drain, so the drop notice precedes the end marker
                        io::nap(1500);
                        let _ = io::write_all(tty_mine, b"flood: end\n");
                    } else {
                        let mut out = Vec::from(&b"got: "[..]);
                        out.extend_from_slice(&cmd);
                        out.push(b'\n');
                        let _ = io::write_all(tty_mine, &out);
                    }
                }
                _ => line.push(b),
            }
        }
    }
    if let Some(j) = job.take() {
        sys::process_kill(j, -1);
        sys::handle_close(j);
    }
    // closing the terminal ends the console server; wait until it has flushed the serial line
    sys::handle_close(tty_mine);
    sys::handle_close(ctl_mine);
    let w = sys::object_wait(
        consoled,
        rt::abi::signals::TERMINATED,
        sys::clock_get() + 5000,
    );
    let code = if w.status == 0 {
        sys::process_info(consoled).a1 as i64
    } else {
        -1
    };
    sys::handle_close(consoled);
    if code == 0 {
        0
    } else {
        25
    }
}

/// The services of one interactive session, in start order.
struct Stack {
    /// The job every process of the session lives in (handle with every right): `stop_stack` ends
    /// whatever is left in it, and its limits are the whole session's.
    session: u32,
    memfsd: u32,
    launcher: u32,
    consoled: u32,
    sh: u32,
    /// `sh` writes here when the user says `halt`.
    halt: u32,
}

fn kill_close(h: u32) {
    rt::sys::process_kill(h, -1);
    rt::sys::handle_close(h);
}

fn start_stack(
    serial: u32,
    image_vmo: u32,
    img: &[u8],
    entries: &[proto::bootimg::Entry],
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
    let root = sys::job_root();
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
    let apps = sys::job_create(session, &[16, 32, 40960, 20, 1000, 768 * 1024]);
    if apps.status != 0 {
        sys::handle_close(session);
        return Err(38);
    }
    let apps = apps.a1 as u32;
    let start = |name: &[u8], roles: Vec<(Role, u32)>, handles: &[u32]| -> Result<u32, i32> {
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
    let memfsd = start(
        b"memfsd",
        alloc::vec![(Role::ServiceFs, 0), (Role::BootImage, 1)],
        &[fs_theirs, image_dup.a1 as u32],
    )?;
    let mut root = rt::fs::Fs::new(fs_mine);
    let sh_fs = match root.clone_conn(proto::file::rights::READ | proto::file::rights::ENUMERATE) {
        Ok(f) => f,
        Err(e) => {
            sys::debug_write(alloc::format!("init: clone of the root failed: {e}\n").as_bytes());
            return Err(35);
        }
    };
    root.close();
    // the console server: the serial line, the terminal socket, the control channel
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
    let consoled = start(
        b"consoled",
        alloc::vec![(Role::Serial, 0), (Role::Tty, 1), (Role::ServiceConsole, 2)],
        &[serial_dup.a1 as u32, tty_console, ctl_console],
    )?;
    let (halt_init, halt_sh) = chan()?;
    let sh = start(
        b"sh",
        alloc::vec![
            (Role::Tty, 0),
            (Role::ServiceFs, 1),
            (Role::ServiceLaunch, 2),
            (Role::ServiceConsole, 3),
            (Role::ServiceInit, 4)
        ],
        &[tty_sh, sh_fs.ch, launch_sh, ctl_sh, halt_sh],
    )?;
    Ok(Stack {
        session,
        memfsd,
        launcher,
        consoled,
        sh,
        halt: halt_init,
    })
}

/// End a session: stop the shell first (which closes the terminal and so ends the console server
/// once it has flushed the serial line), then the rest.
fn stop_stack(s: Stack) {
    use rt::sys;
    sys::process_kill(s.sh, -1);
    let w = sys::object_wait(
        s.consoled,
        rt::abi::signals::TERMINATED,
        sys::clock_get() + 2000,
    );
    let _ = w;
    for h in [s.sh, s.consoled, s.launcher, s.memfsd] {
        kill_close(h);
    }
    // Whatever else the session started (programs the shell left behind) goes too, and nothing
    // can start in it any more.
    sys::job_kill(s.session, -1);
    sys::handle_close(s.session);
    sys::handle_close(s.halt);
}

/// The supervisor (S4-SPEC section 10): start `memfsd`, `launcher`, `consoled` and `sh`; when the
/// shell exits (or any service dies) end the session and start a fresh one; on `halt` end it and
/// return 0, which lets the kernel run its leak checks and power off.
fn supervise(serial: u32, image_vmo: u32, img: &[u8], entries: &[proto::bootimg::Entry]) -> i32 {
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
        let stack = match start_stack(serial, image_vmo, img, entries) {
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
        let watched = [stack.sh, stack.consoled, stack.launcher, stack.memfsd];
        for (i, &h) in watched.iter().enumerate() {
            sys::port_bind(port, h, signals::TERMINATED, i as u64);
        }
        sys::port_bind(port, stack.halt, signals::READABLE, 9);
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
                    stack.halt,
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

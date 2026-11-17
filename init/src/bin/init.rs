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
        let recovery = env.startup.args.get(1).map(|a| a.as_slice()) == Some(b"recovery");
        return supervise(serial, image_vmo, &img, &entries, recovery);
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
                && !e.name.starts_with(b"etc/profiles/")
                && !e.name.starts_with(b"homes/")
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
                if line.as_slice() == b"sessions" {
                    // test aid for UV5: sessions of two profiles side by side, each on a terminal socket
                    return sessions_test(serial, image_vmo, &img, &entries);
                }
                if line.as_slice() == b"detach" {
                    // test aid for UV7: a terminal backend that closes and a new one that attaches
                    return detach_test(serial, image_vmo, &img, &entries);
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
/// UV4: `raw`, `cbreak` and `size` switch the terminal's mode (see `terminal_demo`), `resize` sets the window size
/// and reads it back.
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
                    } else if cmd.as_slice() == b"raw" || cmd.as_slice() == b"cbreak" {
                        let m = if cmd.as_slice() == b"raw" {
                            proto::console::mode::RAW
                        } else {
                            proto::console::mode::CBREAK
                        };
                        let name = if m == 0 { "raw" } else { "cbreak" };
                        let msg = alloc::format!("mode: {name}\n");
                        terminal_demo(tty_mine, ctl_mine, &mut txid, m, false, &msg);
                    } else if cmd.as_slice() == b"size" {
                        let m = proto::console::mode::RAW;
                        terminal_demo(tty_mine, ctl_mine, &mut txid, m, true, "size: asked\n");
                    } else if cmd.as_slice() == b"resize" {
                        use proto::console::Control;
                        let _ = ctl_call(
                            ctl_mine,
                            &mut txid,
                            Control::SetSize {
                                cols: 100,
                                rows: 30,
                            },
                        );
                        let v = ctl_call(ctl_mine, &mut txid, Control::GetSize).unwrap_or(0);
                        let msg = alloc::format!("size: {}x{}\n", v & 0xffff, v >> 16);
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

/// The scripts the UV5 test runs in each session (a shell with no console server reads its script from the terminal socket).
const SESSION_SCRIPTS: &[(&str, &str)] = &[
    (
        "alice",
        "print $env.USER $env.UID $env.HOME $env.EDITOR\nprint (pwd)\ncat secret.txt\ncat /home/secret.txt\nr = run { \"out\": \"capture\" } cat /homes/bob/notes.txt\nprint $r.status $r.out\nset r = run { \"out\": \"capture\" } cat /etc/profiles/bob.json\nprint $r.status $r.out\nset r = run { \"out\": \"capture\" } cat /home/notes.txt\nprint $r.status\nls /\nps\n",
    ),
    (
        "bob",
        "print $env.USER $env.UID\ncat notes.txt\nr = run { \"out\": \"capture\" } cat /homes/alice/secret.txt\nprint $r.status $r.out\nset r = run { \"out\": \"capture\" } cat /home/secret.txt\nprint $r.status\nps\n",
    ),
];

/// Read what the shell of a session writes, line by line (the callback returns false to stop), until it ends.
fn read_session_lines(sock: u32, mut each: impl FnMut(&str) -> bool) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        while let Some(i) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            if !each(String::from_utf8_lossy(&line[..line.len() - 1]).as_ref()) {
                return;
            }
        }
        match io::read(sock, &mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    if !buf.is_empty() {
        each(String::from_utf8_lossy(&buf).as_ref());
    }
}

/// UV5 test aid: run a script in a session of each profile, each on a terminal socket the aid holds (so the shell is in its
/// non-interactive mode), and print what came back, line by line, to the serial line. Then a session whose shell runs a
/// program forever, ended by closing the session: its job must be empty afterwards.
fn sessions_test(
    serial: u32,
    image_vmo: u32,
    img: &[u8],
    entries: &[proto::bootimg::Entry],
) -> i32 {
    use rt::sys;
    let say = |s: &str| {
        let _ = io::write_all(serial, s.as_bytes());
    };
    let profiles = load_profiles(img, entries);
    say("sessions ready\r\n");
    let open = |who: &Loaded| -> Option<(Stack, u32)> {
        let s = sys::socket_create();
        if s.status != 0 {
            return None;
        }
        let (mine, theirs) = (s.a1 as u32, s.a2 as u32);
        let stack = start_stack(Term::Socket(theirs), image_vmo, img, entries, who).ok()?;
        Some((stack, mine))
    };
    for (name, script) in SESSION_SCRIPTS {
        let Some(who) = profiles.iter().find(|l| l.profile.name == *name) else {
            say("no such profile\r\n");
            return 20;
        };
        let Some((stack, mine)) = open(who) else {
            say("the session could not start\r\n");
            return 21;
        };
        let _ = io::write_all(mine, script.as_bytes());
        sys::socket_shutdown(mine);
        read_session_lines(mine, |l| {
            say(&alloc::format!("{name}| {l}\r\n"));
            true
        });
        let w = sys::object_wait(
            stack.sh,
            rt::abi::signals::TERMINATED,
            sys::clock_get() + 5000,
        );
        let code = if w.status == 0 {
            sys::process_info(stack.sh).a1 as i64
        } else {
            -999
        };
        say(&alloc::format!("{name} status {code}\r\n"));
        sys::handle_close(mine);
        stop_stack(stack);
    }
    // closing a session ends its job
    let Some(who) = profiles.iter().find(|l| l.profile.name == "alice") else {
        return 22;
    };
    let Some((stack, mine)) = open(who) else {
        return 23;
    };
    let _ = io::write_all(mine, b"print started\nspin\n");
    sys::socket_shutdown(mine);
    read_session_lines(mine, |l| {
        say(&alloc::format!("alice2| {l}\r\n"));
        l != "spinning"
    });
    let job = sys::handle_duplicate(stack.session, rt::abi::rights::Rights::SAME.0);
    let running = sys::job_info(job.a1 as u32, 0).a1;
    say(&alloc::format!(
        "job processes while open: {}\r\n",
        if running >= 5 {
            "5 or more"
        } else {
            "fewer than 5"
        }
    ));
    stop_stack(stack);
    // (processes are reaped a moment after the kill)
    let mut left = sys::job_info(job.a1 as u32, 0).a1;
    for _ in 0..100 {
        if left == 0 {
            break;
        }
        io::nap(20);
        left = sys::job_info(job.a1 as u32, 0).a1;
    }
    say(&alloc::format!("job processes after close: {left}\r\n"));
    sys::handle_close(mine);
    sys::handle_close(job.a1 as u32);
    0
}

/// One control call to `consoled`: the reply's value (0 for a plain reply), `None` when it failed. Events that arrive
/// meanwhile are dropped (the caller is not looking for them).
fn ctl_call(ctl: u32, txid: &mut u32, c: proto::console::Control) -> Option<u32> {
    use rt::sys;
    *txid += 1;
    let msg = proto::console::encode(*txid, c);
    if sys::channel_write(ctl, msg.as_ptr(), msg.len(), core::ptr::null(), 0).status != 0 {
        return None;
    }
    let mut rb = [0u8; 16];
    for _ in 0..400 {
        let r = sys::channel_read(ctl, rb.as_mut_ptr(), rb.len(), core::ptr::null_mut(), 0);
        if r.status == 0 {
            let b = &rb[..r.a1];
            if let Some((t, st, v)) = proto::console::decode_reply_value(b) {
                return (t == *txid && st == 0).then_some(v);
            }
            if let Some((t, st)) = proto::console::decode_reply(b) {
                return (t == *txid && st == 0).then_some(0);
            }
            continue; // an event
        }
        io::nap(5);
    }
    None
}

/// UV4 test aid: the stand-in shell takes the terminal in `mode` (asking the host for its size first if `query`),
/// says `first`, then reports what happens, one line each: `raw: <hex>` for every byte the terminal delivers and
/// `event: resize CxR`, `event: interrupt` or `event: hangup` for the events. The byte `q` (the carriage return when
/// echo is on) ends it: the stand-in prints the window size and mode it reads back, restores cooked mode and prints
/// `mode: cooked`.
fn terminal_demo(tty: u32, ctl: u32, txid: &mut u32, mode: u8, query: bool, first: &str) {
    use proto::console::{Control, Event};
    use rt::abi::status::Status;
    use rt::sys;
    let quit = if mode & proto::console::mode::ECHO != 0 {
        b'\r'
    } else {
        b'q'
    };
    if ctl_call(ctl, txid, Control::SetMode(mode)).is_none() {
        let _ = io::write_all(tty, b"mode: set failed\n");
        return;
    }
    if query && ctl_call(ctl, txid, Control::QuerySize).is_none() {
        let _ = io::write_all(tty, b"size: query failed\n");
        return;
    }
    let _ = io::write_all(tty, first.as_bytes());
    let mut buf = [0u8; 64];
    'demo: loop {
        let mut idle = true;
        let r = sys::socket_read(tty, buf.as_mut_ptr(), buf.len());
        match r.status() {
            Some(Status::Ok) => {
                idle = false;
                for &b in &buf[..r.a1] {
                    if b == quit {
                        break 'demo;
                    }
                    let _ = io::write_all(tty, alloc::format!("raw: {b:02x}\n").as_bytes());
                }
            }
            Some(Status::ShouldWait) => {}
            _ => return,
        }
        let mut rb = [0u8; 16];
        let r = sys::channel_read(ctl, rb.as_mut_ptr(), rb.len(), core::ptr::null_mut(), 0);
        if r.status == 0 {
            idle = false;
            let line = match proto::console::decode_event(&rb[..r.a1]) {
                Some(Event::Resize { cols, rows }) => {
                    alloc::format!("event: resize {cols}x{rows}\n")
                }
                Some(Event::Interrupt) => String::from("event: interrupt\n"),
                Some(Event::Hangup) => String::from("event: hangup\n"),
                None => String::from("event: ?\n"),
            };
            let _ = io::write_all(tty, line.as_bytes());
        }
        if idle {
            io::nap(5);
        }
    }
    let size = ctl_call(ctl, txid, Control::GetSize).unwrap_or(0);
    let m = ctl_call(ctl, txid, Control::GetMode).unwrap_or(99);
    let _ = ctl_call(ctl, txid, Control::SetMode(proto::console::mode::COOKED));
    let msg = alloc::format!(
        "size: {}x{}\nmode: {m}\nmode: cooked\n",
        size & 0xffff,
        size >> 16
    );
    let _ = io::write_all(tty, msg.as_bytes());
}

/// A profile as read from the image, with its text (the shell is given it to read its own grants).
#[derive(Clone)]
struct Loaded {
    profile: profile::Profile,
    text: String,
}

/// Every profile in the boot image's `etc/profiles/` (a profile that does not parse is skipped, and said so).
fn load_profiles(img: &[u8], entries: &[proto::bootimg::Entry]) -> Vec<Loaded> {
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
enum Term {
    Serial(u32),
    /// A byte socket a backend provider holds the other end of (UV7): the console server serves it, treats its closing as a
    /// detach and takes a new one on the stack's `term` channel.
    Backend(u32),
    Socket(u32),
}

/// The services of one interactive session, in start order.
struct Stack {
    /// The job every process of the session lives in (handle with every right): `stop_stack` ends
    /// whatever is left in it, and its limits are the whole session's.
    session: u32,
    memfsd: u32,
    launcher: u32,
    consoled: Option<u32>,
    sh: u32,
    /// `sh` writes here when the user says `halt`.
    halt: Option<u32>,
    /// A connection to the service registry, to ask the volumes to commit before the session ends.
    registry: Option<rt::fs::Fs>,
    /// Init's end of the console server's attach channel (a `Term::Backend` session): `proto::console::term`.
    term: Option<u32>,
}

fn kill_close(h: u32) {
    rt::sys::process_kill(h, -1);
    rt::sys::handle_close(h);
}

fn start_stack(
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
        let res = sys::resource_root();
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
fn stop_stack(mut s: Stack) {
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
fn supervise(
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
        let stack = match start_stack(Term::Serial(serial), image_vmo, img, entries, &who) {
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
struct Authd {
    ch: u32,
}

fn start_authd(
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
    fn ask(&self, req: &str) -> String {
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

    fn count(&self) -> usize {
        self.ask("count").parse().unwrap_or(0)
    }
}

/// The login prompts, on the serial line (before the console server of a session exists, and after it is gone).
struct Login {
    serial: u32,
    pending: Vec<u8>,
}

impl Login {
    fn say(&self, text: &str) {
        let _ = io::write_all(self.serial, text.as_bytes());
    }

    /// Show `prompt`, read one line (echoed unless `secret`); `None` when the terminal is gone.
    fn line(&mut self, prompt: &str, secret: bool) -> Option<String> {
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
fn setup_profile(name: &str) -> Option<Loaded> {
    let text = alloc::format!("{{ \"name\": \"{name}\", \"uid\": 1000, \"home\": \"home\" }}");
    let p = profile::Profile::parse(&text).ok()?;
    Some(Loaded { profile: p, text })
}

/// Decide who gets a session now (the boot policy of `auth::boot`), prompting as the plan says. `Err` is an exit code.
fn choose_profile(
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

/// Read from a socket until `needle` has arrived (everything read so far is returned), or `ms` have passed.
fn read_until(sock: u32, needle: &[u8], ms: u64) -> Option<Vec<u8>> {
    use rt::abi::signals;
    use rt::sys;
    let deadline = sys::clock_get() + ms;
    let mut got: Vec<u8> = Vec::new();
    loop {
        if got.windows(needle.len()).any(|w| w == needle) {
            return Some(got);
        }
        let mut buf = [0u8; 256];
        let r = sys::socket_read(sock, buf.as_mut_ptr(), buf.len());
        match r.status() {
            Some(rt::abi::status::Status::Ok) if r.a1 > 0 => got.extend_from_slice(&buf[..r.a1]),
            Some(rt::abi::status::Status::ShouldWait) => {
                if sys::clock_get() >= deadline {
                    return None;
                }
                sys::object_wait(sock, signals::READABLE | signals::PEER_CLOSED, deadline);
            }
            _ => return None,
        }
    }
}

/// UV7 acceptance, driven from `init` as a terminal backend provider: a session whose terminal is served by `consoled` over a
/// byte socket we hold. A long job runs; the backend closes (a detach: the job and the session carry on); a new backend
/// attaches and is shown what the terminal last displayed, and can use it (Ctrl-C reaches the job); then a hangup ends it.
fn detach_test(serial: u32, image_vmo: u32, img: &[u8], entries: &[proto::bootimg::Entry]) -> i32 {
    use rt::abi::rights::Rights;
    use rt::abi::signals;
    use rt::sys;
    let say = |s: &str| {
        let _ = io::write_all(serial, alloc::format!("detach| {s}\r\n").as_bytes());
    };
    let profiles = load_profiles(img, entries);
    let Some(who) = profiles.iter().find(|l| l.profile.name == "alice") else {
        return 20;
    };
    let pair = || -> Option<(u32, u32)> {
        let s = sys::socket_create();
        (s.status == 0).then_some((s.a1 as u32, s.a2 as u32))
    };
    let Some((mut mine, theirs)) = pair() else {
        return 21;
    };
    let Ok(stack) = start_stack(Term::Backend(theirs), image_vmo, img, entries, who) else {
        say("the session could not start");
        return 22;
    };
    sys::handle_close(theirs);
    let Some(term) = stack.term else {
        return 23;
    };
    let job = sys::handle_duplicate(stack.session, Rights::SAME.0).a1 as u32;
    let mut txid = 0u32;
    let mut call = |op: u16, handle: Option<u32>| -> bool {
        txid += 1;
        let msg = proto::console::term::encode(txid, op);
        let hs: &[u32] = match &handle {
            Some(h) => core::slice::from_ref(h),
            None => &[],
        };
        if sys::channel_write(term, msg.as_ptr(), msg.len(), hs.as_ptr(), hs.len()).status != 0 {
            return false;
        }
        if sys::object_wait(term, signals::READABLE, sys::clock_get() + 3000).status != 0 {
            return false;
        }
        let mut b = [0u8; 16];
        let r = sys::channel_read(term, b.as_mut_ptr(), b.len(), core::ptr::null_mut(), 0);
        r.status == 0 && proto::console::decode_reply(&b[..r.a1]) == Some((txid, 0))
    };
    let ended = |h: u32| sys::object_wait(h, signals::TERMINATED, 0).status == 0;

    // 1. a backend is attached: the shell's prompt, then a long job that prints once
    if read_until(mine, b"$ ", 8000).is_none() {
        say("no prompt");
        return 24;
    }
    let _ = io::write_all(mine, b"spin\r");
    if read_until(mine, b"spinning", 8000).is_none() {
        say("the job did not start");
        return 25;
    }
    say("attached: a prompt, and a job running");
    let before = sys::job_info(job, 0).a1;

    // 2. the backend closes: a detach, not a hangup
    sys::handle_close(mine);
    io::nap(700);
    say(&alloc::format!(
        "backend closed: the shell {}, the console server {}, the job {}",
        if ended(stack.sh) { "ended" } else { "runs" },
        if stack.consoled.is_some_and(ended) {
            "ended"
        } else {
            "runs"
        },
        if sys::job_info(job, 0).a1 >= before {
            "runs"
        } else {
            "lost processes"
        }
    ));

    // 3. a new backend attaches: it is shown what the terminal last displayed, and it works
    let Some((m2, t2)) = pair() else {
        return 26;
    };
    mine = m2;
    if !call(proto::console::term::OP_ATTACH, Some(t2)) {
        say("the attach was refused");
        return 27;
    }
    if read_until(mine, b"spinning", 5000).is_some() {
        say("reattached: the screen state is shown again");
    } else {
        say("reattached: nothing was shown");
    }
    let _ = io::write_all(mine, b"\x03");
    if read_until(mine, b"[exit -130]", 8000).is_some() {
        say("the new backend's Ctrl-C reached the job");
    } else {
        say("the new backend's Ctrl-C did not reach the job");
    }
    let _ = io::write_all(mine, b"echo alive\r");
    if read_until(mine, b"alive\r\n", 5000).is_some() {
        say("the shell answers the new backend");
    } else {
        say("the shell does not answer");
    }

    // 4. a hangup ends it, job and all
    let _ = io::write_all(mine, b"spin\r");
    let _ = read_until(mine, b"spinning", 5000);
    let hung = call(proto::console::term::OP_HANGUP, None);
    let w = sys::object_wait(stack.sh, signals::TERMINATED, sys::clock_get() + 5000);
    say(&alloc::format!(
        "hangup: {}, the shell {}",
        if hung {
            "acknowledged"
        } else {
            "not acknowledged"
        },
        if w.status == 0 { "ended" } else { "still runs" }
    ));
    sys::handle_close(mine);
    stop_stack(stack);
    let mut left = sys::job_info(job, 0).a1;
    for _ in 0..100 {
        if left == 0 {
            break;
        }
        io::nap(20);
        left = sys::job_info(job, 0).a1;
    }
    say(&alloc::format!("job processes after the hangup: {left}"));
    sys::handle_close(job);
    0
}

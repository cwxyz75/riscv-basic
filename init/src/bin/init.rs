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

#[path = "../session.rs"]
mod session;
use session::*;

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
    if env.startup.args.get(1).map(|a| a.as_slice()) != Some(b"repl") {
        // production: `init` reads only the image's table and the session manager's program, never the whole image
        let recovery = env.startup.args.get(1).map(|a| a.as_slice()) == Some(b"recovery");
        return run_sessiond(serial, image_vmo, recovery);
    }
    let Some(img) = read_image(image_vmo) else {
        return 4;
    };
    let Ok(entries) = proto::bootimg::parse(&img) else {
        return 5;
    };
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
/// The authority `init` alone has, for the test aids that build sessions themselves.
fn own_powers() -> Option<Powers> {
    let job = rt::sys::job_root();
    let res = rt::sys::resource_root();
    (job.status == 0 && res.status == 0).then_some(Powers {
        root_job: job.a1 as u32,
        resource: res.a1 as u32,
    })
}

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
    let Some(powers) = own_powers() else {
        return 19;
    };
    say("sessions ready\r\n");
    let open = |who: &Loaded| -> Option<(Stack, u32)> {
        let s = sys::socket_create();
        if s.status != 0 {
            return None;
        }
        let (mine, theirs) = (s.a1 as u32, s.a2 as u32);
        let stack =
            start_stack(&powers, Term::Socket(theirs), image_vmo, img, entries, who).ok()?;
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
    let Some(powers) = own_powers() else {
        return 19;
    };
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
    let Ok(stack) = start_stack(&powers, Term::Backend(theirs), image_vmo, img, entries, who)
    else {
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

/// Start the session manager and wait for it (T128). `init` keeps the kernel's first-process authority and gives `sessiond`
/// handles to the root job, the root resource, the boot image and the serial line, and nothing else: it never holds a profile
/// text, a password or a verifier. `sessiond` ends the system by exiting 0 (a `halt`); any other code is a failure.
fn run_sessiond(serial: u32, image_vmo: u32, recovery: bool) -> i32 {
    use rt::abi::rights::Rights;
    use rt::sys;
    // the table is at the start of the image; the program is read from its extent
    let total = sys::vmo_get_size(image_vmo);
    if total.status != 0 {
        return 4;
    }
    let mut head = alloc::vec![0u8; total.a1.min(64 * 1024)];
    if sys::vmo_read(image_vmo, 0, head.as_mut_ptr(), head.len()).status != 0 {
        return 4;
    }
    let Ok(entries) = proto::bootimg::parse_prefix(&head, total.a1) else {
        return 5;
    };
    let Some(e) = entries.iter().find(|e| e.name == b"bin/sessiond") else {
        return 50;
    };
    let mut bin = alloc::vec![0u8; e.size as usize];
    for (i, chunk) in bin.chunks_mut(32 * 1024).enumerate() {
        let at = u64::from(e.offset) + (i * 32 * 1024) as u64;
        if sys::vmo_read(image_vmo, at, chunk.as_mut_ptr(), chunk.len()).status != 0 {
            return 50;
        }
    }
    let job = sys::job_root();
    let res = sys::resource_root();
    if job.status != 0 || res.status != 0 {
        return 51;
    }
    let dup = |h: u32| {
        let d = sys::handle_duplicate(h, Rights::SAME.0);
        (d.status == 0).then_some(d.a1 as u32)
    };
    let (Some(image), Some(ser), Some(j), Some(r)) = (
        dup(image_vmo),
        dup(serial),
        dup(job.a1 as u32),
        dup(res.a1 as u32),
    ) else {
        return 52;
    };
    let startup = proto::startup::Startup {
        args: alloc::vec![b"sessiond".to_vec()],
        env: if recovery {
            alloc::vec![b"RECOVERY=1".to_vec()]
        } else {
            Vec::new()
        },
        roles: alloc::vec![
            (Role::BootImage, 0),
            (Role::Serial, 1),
            (Role::Job, 2),
            (Role::Resource, 3)
        ],
        ..proto::startup::Startup::default()
    };
    let Ok(bytes) = startup.encode() else {
        return 53;
    };
    let Ok(p) = rt::spawn::spawn(&bin, &bytes, &[image, ser, j, r]) else {
        return 54;
    };
    let w = sys::object_wait(p, rt::abi::signals::TERMINATED, u64::MAX);
    if w.status != 0 {
        return 55;
    }
    sys::process_info(p).a1 as i64 as i32
}

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
    for e in &entries {
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
    let (in_w, in_r) = {
        let s = sys::socket_create();
        (s.a1 as u32, s.a2 as u32)
    };
    let (out_r, out_w) = {
        let s = sys::socket_create();
        (s.a1 as u32, s.a2 as u32)
    };
    let (Some(in_r), Some(out_w)) = (
        narrow(in_r, Rights::READ.0 | Rights::WAIT.0 | Rights::TRANSFER.0),
        narrow(out_w, Rights::WRITE.0 | Rights::WAIT.0 | Rights::TRANSFER.0),
    ) else {
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
    sys::object_wait(proc, rt::abi::signals::TERMINATED, u64::MAX);
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

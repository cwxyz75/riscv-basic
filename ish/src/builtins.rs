//! The built-in functions of `ish` (SHELL-SPEC sections 5 to 7). A builtin takes values and returns a value; the data
//! primitives are thin wrappers over the `data` crate.

use crate::eval::*;
use crate::host::*;
use crate::value::{VMap, Val};
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use data::{Limits, Number, TableOptions};

const NAMES: &[&str] = &[
    "print",
    "eprint",
    "ok",
    "fail",
    "exit",
    "break",
    "continue",
    "call",
    "type",
    "len",
    "keys",
    "values",
    "has",
    "get-or",
    "require",
    "set-in",
    "del-in",
    "append",
    "prepend",
    "nth",
    "slice",
    "join",
    "split",
    "list",
    "map",
    "filter",
    "fold",
    "each",
    "take",
    "sort",
    "reverse",
    "contains",
    "starts-with",
    "ends-with",
    "upper",
    "lower",
    "trim",
    "replace",
    "from-json",
    "to-json",
    "from-csv",
    "to-csv",
    "from-tsv",
    "to-tsv",
    "to-number",
    "str",
    "format",
    "read-file",
    "write-file",
    "export",
    "cd",
    "pwd",
    "import",
    "lines-of",
    "lines",
    "~",
    "range",
    "succeeds",
    "run",
    "help",
    "halt",
    "ns",
    "confine",
];

pub fn is_builtin(name: &str) -> bool {
    NAMES.contains(&name)
}

// ---- text of values ----

pub fn json_text(v: &Val) -> String {
    match v.to_data() {
        Ok(d) => data::to_json(&d, &Limits::DEFAULT).unwrap_or_else(|_| String::from("<too deep>")),
        Err(_) => String::from("<closure>"),
    }
}

/// One value as the text `print` shows: strings raw, everything else as JSON.
pub fn echo_line(v: &Val) -> String {
    match v {
        Val::Str(s) => String::from(&**s),
        Val::Num(n) => n.format(),
        Val::Bool(b) => b.to_string(),
        Val::Null => String::from("null"),
        v => json_text(v),
    }
}

/// What the interactive prompt shows for a value: a list of strings as lines, anything else as one line.
pub fn echo_text(v: &Val) -> String {
    if let Val::List(l) = v {
        if !l.is_empty() && l.iter().all(|x| matches!(x, Val::Str(_))) {
            let mut out = String::new();
            for x in l.iter() {
                out.push_str(&echo_line(x));
                out.push('\n');
            }
            return out;
        }
    }
    let mut t = echo_line(v);
    t.push('\n');
    t
}

// ---- paths and indexing ----

#[derive(Clone)]
enum Key {
    Name(String),
    Idx(usize),
}

fn key_of(v: &Val) -> Res<Key> {
    match v {
        Val::Str(s) => Ok(Key::Name(String::from(&**s))),
        Val::Num(Number::Int(i)) if *i >= 0 => Ok(Key::Idx(*i as usize)),
        v => Err(error(&format!(
            "a key must be a string or a non-negative integer, got a {}",
            v.kind()
        ))),
    }
}

fn missing(what: &str) -> Flow {
    Flow::Fail(Fail {
        value: Val::record(vec![
            ("kind", Val::str("missing-key")),
            ("message", Val::string(format!("no such key: {what}"))),
        ]),
        code: 1,
        kind: FailKind::Error,
    })
}

fn keys_of(path: &Val) -> Res<Vec<Key>> {
    match path {
        Val::List(l) => l.iter().map(key_of).collect(),
        one => Ok(vec![key_of(one)?]),
    }
}

/// `$v.key` / `$v.0`: a map entry, or a list element (a number, or digits).
pub fn index(v: &Val, key: &Val) -> Res<Val> {
    let k = key_of(key)?;
    step(v, &k)
}

fn step(v: &Val, k: &Key) -> Res<Val> {
    match (v, k) {
        (Val::Map(m), Key::Name(n)) => m.get(n).cloned().ok_or_else(|| missing(n)),
        (Val::Map(m), Key::Idx(i)) => m
            .get(&i.to_string())
            .cloned()
            .ok_or_else(|| missing(&i.to_string())),
        (Val::List(l), Key::Idx(i)) => l.get(*i).cloned().ok_or_else(|| missing(&i.to_string())),
        (Val::List(l), Key::Name(n)) => match n.parse::<usize>() {
            Ok(i) => l.get(i).cloned().ok_or_else(|| missing(n)),
            Err(_) => Err(error(&format!("a list is indexed by a number, not `{n}`"))),
        },
        (v, _) => Err(error(&format!("cannot look inside a {}", v.kind()))),
    }
}

fn get_path(v: &Val, keys: &[Key]) -> Res<Val> {
    let mut cur = v.clone();
    for k in keys {
        cur = step(&cur, k)?;
    }
    Ok(cur)
}

/// `v` with the entry at `keys` replaced by `new`. A map key that is not there yet is added (last step only); a list
/// index must exist. Unique owners are updated in place.
pub fn set_path(cur: Val, keys: &[Val], new: Val) -> Res<Val> {
    let ks: Vec<Key> = keys.iter().map(key_of).collect::<Res<_>>()?;
    set_in(cur, &ks, new)
}

fn set_in(cur: Val, keys: &[Key], new: Val) -> Res<Val> {
    let Some((k, rest)) = keys.split_first() else {
        return Ok(new);
    };
    match (cur, k) {
        (Val::Map(mut rc), k @ (Key::Name(_) | Key::Idx(_))) => {
            let name = match k {
                Key::Name(n) => n.clone(),
                Key::Idx(i) => i.to_string(),
            };
            let m = Rc::make_mut(&mut rc);
            if rest.is_empty() {
                m.set(&name, new);
            } else {
                let child = match m.get(&name) {
                    Some(c) => c.clone(),
                    None => return Err(missing(&name)),
                };
                m.set(&name, Val::Null); // drop the map's hold so the child can be updated in place
                let nv = set_in(child, rest, new)?;
                m.set(&name, nv);
            }
            Ok(Val::Map(rc))
        }
        (Val::List(mut rc), k) => {
            let i = match k {
                Key::Idx(i) => *i,
                Key::Name(n) => n
                    .parse::<usize>()
                    .map_err(|_| error(&format!("a list is indexed by a number, not `{n}`")))?,
            };
            let l = Rc::make_mut(&mut rc);
            if i >= l.len() {
                return Err(missing(&i.to_string()));
            }
            let child = core::mem::replace(&mut l[i], Val::Null);
            l[i] = set_in(child, rest, new)?;
            Ok(Val::List(rc))
        }
        (v, _) => Err(error(&format!("cannot set inside a {}", v.kind()))),
    }
}

fn del_in(cur: Val, keys: &[Key]) -> Res<Val> {
    let Some((k, rest)) = keys.split_first() else {
        return Err(error("del-in needs a key"));
    };
    match (cur, k) {
        (Val::Map(mut rc), k @ (Key::Name(_) | Key::Idx(_))) => {
            let name = match k {
                Key::Name(n) => n.clone(),
                Key::Idx(i) => i.to_string(),
            };
            let m = Rc::make_mut(&mut rc);
            if rest.is_empty() {
                if !m.remove(&name) {
                    return Err(missing(&name));
                }
            } else {
                let child = m.get(&name).cloned().ok_or_else(|| missing(&name))?;
                m.set(&name, Val::Null);
                let nv = del_in(child, rest)?;
                m.set(&name, nv);
            }
            Ok(Val::Map(rc))
        }
        (Val::List(mut rc), k) => {
            let i = match k {
                Key::Idx(i) => *i,
                Key::Name(n) => n
                    .parse::<usize>()
                    .map_err(|_| error(&format!("a list is indexed by a number, not `{n}`")))?,
            };
            let l = Rc::make_mut(&mut rc);
            if i >= l.len() {
                return Err(missing(&i.to_string()));
            }
            if rest.is_empty() {
                l.remove(i);
            } else {
                let child = core::mem::replace(&mut l[i], Val::Null);
                l[i] = del_in(child, rest)?;
            }
            Ok(Val::List(rc))
        }
        (v, _) => Err(error(&format!("cannot delete inside a {}", v.kind()))),
    }
}

// ---- patterns ----

/// `*` (any run), `?` (one character); everything else literal. No classes, no escapes.
pub fn glob_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) && p[pi] != '*' {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ---- argument helpers ----

fn arity(name: &str, args: &[Val], lo: usize, hi: usize) -> Res<()> {
    if args.len() < lo || args.len() > hi {
        let want = if lo == hi {
            format!("{lo}")
        } else {
            format!("{lo} to {hi}")
        };
        return Err(error(&format!(
            "{name} takes {want} argument(s), got {}",
            args.len()
        )));
    }
    Ok(())
}

fn string<'a>(name: &str, v: &'a Val) -> Res<&'a str> {
    match v {
        Val::Str(s) => Ok(s),
        v => Err(error(&format!("{name} needs a string, got a {}", v.kind()))),
    }
}

fn list<'a>(name: &str, v: &'a Val) -> Res<&'a Rc<Vec<Val>>> {
    match v {
        Val::List(l) => Ok(l),
        v => Err(error(&format!("{name} needs a list, got a {}", v.kind()))),
    }
}

fn int(name: &str, v: &Val) -> Res<i64> {
    match v {
        Val::Num(Number::Int(i)) => Ok(*i),
        v => Err(error(&format!(
            "{name} needs an integer, got a {}",
            v.kind()
        ))),
    }
}

fn table_opts(name: &str, v: Option<&Val>) -> Res<TableOptions> {
    let mut o = TableOptions::default();
    let Some(v) = v else { return Ok(o) };
    let Val::Map(m) = v else {
        return Err(error(&format!("{name}'s options must be a map")));
    };
    for (k, x) in m.iter() {
        match (k, x) {
            ("header", Val::Bool(b)) => o.header = *b,
            ("crlf", Val::Bool(b)) => o.crlf = *b,
            ("delimiter", Val::Str(s)) if s.len() == 1 && s.is_ascii() => {
                o.delimiter = s.as_bytes()[0]
            }
            _ => return Err(error(&format!("{name}: unknown or bad option `{k}`"))),
        }
    }
    Ok(o)
}

fn data_of(v: &Val) -> Res<data::Value> {
    v.to_data()
        .map_err(|k| error(&format!("a {k} cannot be serialized")))
}

impl<H: Host> Shell<H> {
    /// All of standard input as lines.
    fn stdin_lines(&mut self) -> Res<Vec<Val>> {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match self.host.read(self.io.stdin, &mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if bytes.len() + n > self.cap {
                        return Err(error("input too large"));
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                }
                Err(e) => return Err(error(&format!("read failed: {}", e.message()))),
            }
        }
        let text = String::from_utf8(bytes).map_err(|_| error("input is not valid UTF-8"))?;
        Ok(split_lines(&text).into_iter().map(Val::string).collect())
    }

    /// The list a list-processing builtin works on: the last argument, or standard input.
    fn items(&mut self, args: &[Val], n_before: usize) -> Res<Vec<Val>> {
        match args.get(n_before) {
            Some(Val::List(l)) => Ok(l.iter().cloned().collect()),
            Some(v) => Err(error(&format!("expected a list, got a {}", v.kind()))),
            None => self.stdin_lines(),
        }
    }

    fn call_fn(&mut self, f: &Val, args: Vec<Val>) -> Res<Val> {
        match f {
            Val::Closure(c) => self.call_closure(c, args),
            v => Err(error(&format!("expected a function, got a {}", v.kind()))),
        }
    }

    pub(crate) fn call_builtin(&mut self, name: &str, args: Vec<Val>) -> Res<Val> {
        let a = &args;
        match name {
            "print" | "eprint" => {
                let mut line = String::new();
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        line.push(' ');
                    }
                    line.push_str(&echo_line(v));
                }
                line.push('\n');
                if name == "print" {
                    self.out(line.as_bytes())?;
                } else {
                    self.err_out(line.as_bytes())?;
                }
                Ok(Val::Null)
            }
            "ok" => {
                arity(name, a, 0, 1)?;
                Err(Flow::Ok(a.first().cloned().unwrap_or(Val::Null)))
            }
            "fail" => {
                arity(name, a, 1, 2)?;
                let code = match a.get(1) {
                    Some(v) => int(name, v)? as i32,
                    None => 1,
                };
                Err(Flow::Fail(Fail {
                    value: a[0].clone(),
                    code,
                    kind: FailKind::User,
                }))
            }
            "exit" => {
                arity(name, a, 0, 1)?;
                let code = match a.first() {
                    Some(v) => int(name, v)? as i32,
                    None => 0,
                };
                Err(Flow::Exit(code))
            }
            "break" => Err(Flow::Break),
            "continue" => Err(Flow::Continue),
            "call" => {
                if a.is_empty() {
                    return Err(error("call needs a function"));
                }
                self.call_fn(&a[0], a[1..].to_vec())
            }
            "type" => {
                arity(name, a, 1, 1)?;
                Ok(Val::str(a[0].kind()))
            }
            "len" => {
                arity(name, a, 1, 1)?;
                match &a[0] {
                    Val::List(l) => Ok(Val::int(l.len() as i64)),
                    Val::Map(m) => Ok(Val::int(m.len() as i64)),
                    Val::Str(s) => Ok(Val::int(s.chars().count() as i64)),
                    v => Err(error(&format!(
                        "len needs a list, map or string, got a {}",
                        v.kind()
                    ))),
                }
            }
            "keys" => {
                arity(name, a, 1, 1)?;
                match &a[0] {
                    Val::Map(m) => Ok(Val::list(m.iter().map(|(k, _)| Val::str(k)).collect())),
                    v => Err(error(&format!("keys needs a map, got a {}", v.kind()))),
                }
            }
            "values" => {
                arity(name, a, 1, 1)?;
                match &a[0] {
                    Val::Map(m) => Ok(Val::list(m.iter().map(|(_, v)| v.clone()).collect())),
                    Val::List(l) => Ok(Val::List(l.clone())),
                    v => Err(error(&format!("values needs a map, got a {}", v.kind()))),
                }
            }
            "has" => {
                arity(name, a, 2, 2)?;
                Ok(Val::Bool(get_path(&a[0], &keys_of(&a[1])?).is_ok()))
            }
            "get-or" => {
                arity(name, a, 3, 3)?;
                match get_path(&a[0], &keys_of(&a[1])?) {
                    Ok(v) => Ok(v),
                    Err(Flow::Fail(f)) if matches!(&f.value, Val::Map(m) if m.get("kind").and_then(Val::as_str) == Some("missing-key")) => {
                        Ok(a[2].clone())
                    }
                    Err(e) => Err(e),
                }
            }
            "require" => {
                arity(name, a, 2, 2)?;
                get_path(&a[0], &keys_of(&a[1])?)
            }
            "set-in" => {
                arity(name, a, 3, 3)?;
                set_in(a[0].clone(), &keys_of(&a[1])?, a[2].clone())
            }
            "del-in" => {
                arity(name, a, 2, 2)?;
                del_in(a[0].clone(), &keys_of(&a[1])?)
            }
            "append" => {
                if a.is_empty() {
                    return Err(error("append needs a list"));
                }
                let mut l = list(name, &a[0])?.as_ref().clone();
                l.extend(a[1..].iter().cloned());
                Ok(Val::list(l))
            }
            "prepend" => {
                if a.is_empty() {
                    return Err(error("prepend needs a list"));
                }
                let mut l: Vec<Val> = a[1..].to_vec();
                l.extend(list(name, &a[0])?.iter().cloned());
                Ok(Val::list(l))
            }
            "nth" => {
                arity(name, a, 2, 2)?;
                let i = int(name, &a[0])?;
                let l = list(name, &a[1])?;
                usize::try_from(i)
                    .ok()
                    .and_then(|i| l.get(i))
                    .cloned()
                    .ok_or_else(|| missing(&i.to_string()))
            }
            "slice" => {
                arity(name, a, 2, 3)?;
                let from = int(name, &a[1])?.max(0) as usize;
                let to = match a.get(2) {
                    Some(v) => Some(int(name, v)?.max(0) as usize),
                    None => None,
                };
                match &a[0] {
                    Val::Str(s) => {
                        let cs: Vec<char> = s.chars().collect();
                        let to = to.unwrap_or(cs.len()).min(cs.len());
                        let from = from.min(to);
                        Ok(Val::string(cs[from..to].iter().collect()))
                    }
                    Val::List(l) => {
                        let to = to.unwrap_or(l.len()).min(l.len());
                        let from = from.min(to);
                        Ok(Val::list(l[from..to].to_vec()))
                    }
                    v => Err(error(&format!(
                        "slice needs a string or list, got a {}",
                        v.kind()
                    ))),
                }
            }
            "join" => {
                arity(name, a, 2, 2)?;
                let sep = string(name, &a[0])?;
                let l = list(name, &a[1])?;
                let mut parts = Vec::new();
                for x in l.iter() {
                    match x.word() {
                        Some(t) => parts.push(t),
                        None => {
                            return Err(error(&format!("join: a {} cannot be joined", x.kind())))
                        }
                    }
                }
                Ok(Val::string(parts.join(sep)))
            }
            "split" => {
                arity(name, a, 2, 2)?;
                let sep = string(name, &a[0])?;
                if sep.is_empty() {
                    return Err(error("split needs a separator"));
                }
                Ok(Val::list(
                    string(name, &a[1])?.split(sep).map(Val::str).collect(),
                ))
            }
            "list" => Ok(Val::list(args)),
            "range" => {
                arity(name, a, 1, 2)?;
                let (from, to) = if a.len() == 1 {
                    (0, int(name, &a[0])?)
                } else {
                    (int(name, &a[0])?, int(name, &a[1])?)
                };
                if to - from > 1_000_000 {
                    return Err(error("range too large"));
                }
                Ok(Val::list((from..to).map(Val::int).collect()))
            }
            "map" | "filter" | "each" | "fold" | "take" => self.list_op(name, args),
            "sort" => {
                arity(name, a, 1, 1)?;
                let l = list(name, &a[0])?;
                let mut v: Vec<Val> = l.as_ref().clone();
                if v.iter().all(|x| matches!(x, Val::Str(_))) {
                    v.sort_by(|x, y| x.as_str().cmp(&y.as_str()));
                } else if v.iter().all(|x| matches!(x, Val::Num(_))) {
                    v.sort_by(|x, y| match (x, y) {
                        (Val::Num(p), Val::Num(q)) => {
                            p.compare(q).unwrap_or(core::cmp::Ordering::Equal)
                        }
                        _ => core::cmp::Ordering::Equal,
                    });
                } else {
                    return Err(error("sort needs a list of strings or a list of numbers"));
                }
                Ok(Val::list(v))
            }
            "reverse" => {
                arity(name, a, 1, 1)?;
                let mut v = list(name, &a[0])?.as_ref().clone();
                v.reverse();
                Ok(Val::list(v))
            }
            "contains" => {
                arity(name, a, 2, 2)?;
                match &a[0] {
                    Val::Str(s) => Ok(Val::Bool(s.contains(string(name, &a[1])?))),
                    Val::List(l) => Ok(Val::Bool(l.iter().any(|x| x.equals(&a[1])))),
                    v => Err(error(&format!(
                        "contains needs a string or list, got a {}",
                        v.kind()
                    ))),
                }
            }
            "starts-with" => {
                arity(name, a, 2, 2)?;
                Ok(Val::Bool(
                    string(name, &a[0])?.starts_with(string(name, &a[1])?),
                ))
            }
            "ends-with" => {
                arity(name, a, 2, 2)?;
                Ok(Val::Bool(
                    string(name, &a[0])?.ends_with(string(name, &a[1])?),
                ))
            }
            "upper" => {
                arity(name, a, 1, 1)?;
                Ok(Val::string(string(name, &a[0])?.to_uppercase()))
            }
            "lower" => {
                arity(name, a, 1, 1)?;
                Ok(Val::string(string(name, &a[0])?.to_lowercase()))
            }
            "trim" => {
                arity(name, a, 1, 1)?;
                Ok(Val::str(string(name, &a[0])?.trim()))
            }
            "replace" => {
                arity(name, a, 3, 3)?;
                let from = string(name, &a[1])?;
                if from.is_empty() {
                    return Err(error("replace needs text to look for"));
                }
                Ok(Val::string(
                    string(name, &a[0])?.replace(from, string(name, &a[2])?),
                ))
            }
            "~" => {
                arity(name, a, 2, 2)?;
                Ok(Val::Bool(glob_match(
                    string(name, &a[1])?,
                    &echo_line(&a[0]),
                )))
            }
            "from-json" => {
                arity(name, a, 1, 1)?;
                let text = self.text_of_input(&a[0])?;
                match data::from_json(text.as_bytes(), &Limits::DEFAULT) {
                    Ok(d) => Ok(Val::from_data(&d)),
                    Err(e) => Err(Flow::Fail(Fail {
                        value: Val::record(vec![
                            ("kind", Val::str("bad-json")),
                            ("message", Val::string(e.to_string())),
                        ]),
                        code: 1,
                        kind: FailKind::Error,
                    })),
                }
            }
            "to-json" => {
                arity(name, a, 1, 2)?;
                let d = data_of(&a[0])?;
                let indent = match a.get(1) {
                    Some(Val::Map(m)) => match m.get("indent") {
                        Some(Val::Num(Number::Int(i))) if (0..=16).contains(i) => Some(*i as usize),
                        None => None,
                        _ => return Err(error("to-json: indent must be 0 to 16")),
                    },
                    Some(_) => return Err(error("to-json's options must be a map")),
                    None => None,
                };
                let r = match indent {
                    Some(i) if i > 0 => data::to_json_pretty(&d, i, &Limits::DEFAULT),
                    _ => data::to_json(&d, &Limits::DEFAULT),
                };
                r.map(Val::string).map_err(|e| error(e.message()))
            }
            "from-csv" | "from-tsv" => {
                arity(name, a, 1, 2)?;
                let text = self.text_of_input(&a[0])?;
                let o = table_opts(name, a.get(1))?;
                let r = if name == "from-csv" {
                    data::from_csv(text.as_bytes(), &o)
                } else {
                    data::from_tsv(text.as_bytes(), &o)
                };
                match r {
                    Ok(d) => Ok(Val::from_data(&d)),
                    Err(e) => Err(error(&format!(
                        "{name}: {} at line {}",
                        e.kind.message(),
                        e.line
                    ))),
                }
            }
            "to-csv" | "to-tsv" => {
                arity(name, a, 1, 2)?;
                let d = data_of(&a[0])?;
                let o = table_opts(name, a.get(1))?;
                let r = if name == "to-csv" {
                    data::to_csv(&d, &o)
                } else {
                    data::to_tsv(&d, &o)
                };
                r.map(Val::string)
                    .map_err(|k| error(&format!("{name}: {}", k.message())))
            }
            "to-number" => {
                arity(name, a, 1, 1)?;
                match &a[0] {
                    Val::Num(n) => Ok(Val::Num(*n)),
                    Val::Str(s) => Number::parse(s.as_bytes())
                        .map(Val::Num)
                        .map_err(|_| error(&format!("not a number: {s}"))),
                    v => Err(error(&format!(
                        "to-number needs a string, got a {}",
                        v.kind()
                    ))),
                }
            }
            "str" => {
                arity(name, a, 1, 1)?;
                match a[0].word() {
                    Some(t) => Ok(Val::string(t)),
                    None => Ok(Val::string(json_text(&a[0]))),
                }
            }
            "format" => {
                if a.is_empty() {
                    return Err(error("format needs a template"));
                }
                let t = string(name, &a[0])?;
                let mut out = String::new();
                let mut next = 1;
                let mut it = t.chars().peekable();
                while let Some(c) = it.next() {
                    if c == '%' {
                        match it.next() {
                            Some('s') => {
                                let v = a
                                    .get(next)
                                    .ok_or_else(|| error("format: not enough arguments"))?;
                                out.push_str(&echo_line(v));
                                next += 1;
                            }
                            Some('%') => out.push('%'),
                            _ => return Err(error("format: only %s and %% are known")),
                        }
                    } else {
                        out.push(c);
                    }
                }
                if next != a.len() {
                    return Err(error("format: too many arguments"));
                }
                Ok(Val::string(out))
            }
            "read-file" => {
                arity(name, a, 1, 1)?;
                let p = string(name, &a[0])?;
                let b = self
                    .host
                    .read_file(p)
                    .map_err(|e| error(&format!("{p}: {}", e.message())))?;
                String::from_utf8(b)
                    .map(Val::string)
                    .map_err(|_| error(&format!("{p}: not valid UTF-8")))
            }
            "write-file" => {
                arity(name, a, 2, 2)?;
                let p = string(name, &a[0])?;
                let text = match &a[1] {
                    Val::Str(s) => String::from(&**s),
                    v => echo_line(v),
                };
                self.host
                    .write_file(p, text.as_bytes())
                    .map_err(|e| error(&format!("{p}: {}", e.message())))?;
                Ok(Val::Null)
            }
            "export" => {
                arity(name, a, 2, 2)?;
                let k = string(name, &a[0])?;
                match a[1].word() {
                    Some(v) => self.host.setenv(k, &v),
                    None => return Err(error("export needs a text value")),
                }
                Ok(Val::Null)
            }
            "cd" => {
                if self.legacy && a.len() != 1 {
                    self.out(b"cd: usage: cd <dir>\n")?;
                    return Ok(Val::Null);
                }
                arity(name, a, 1, 1)?;
                let p = string(name, &a[0])?;
                match self.host.chdir(p) {
                    Ok(()) => Ok(Val::Null),
                    Err(e) if self.legacy => {
                        // the S4e shell said it and went on
                        self.out(format!("cd: {p}: {}\n", e.message()).as_bytes())?;
                        Ok(Val::Null)
                    }
                    Err(e) => Err(error(&format!("cd: {p}: {}", e.message()))),
                }
            }
            "ns" => {
                arity(name, a, 0, 0)?;
                let rows: Vec<Val> = self
                    .host
                    .table()
                    .into_iter()
                    .map(|(p, r)| {
                        Val::record(vec![("path", Val::string(p)), ("rights", Val::string(r))])
                    })
                    .collect();
                Ok(Val::list(rows))
            }
            "confine" => {
                arity(name, a, 2, 2)?;
                let Val::Map(m) = &a[0] else {
                    return Err(error(&format!(
                        "confine needs a map of grants, got a {}",
                        a[0].kind()
                    )));
                };
                let mut grants = Vec::new();
                for (path, r) in m.iter() {
                    match r {
                        Val::Str(r) if matches!(&**r, "r" | "rw") => {
                            grants.push((String::from(path), String::from(&**r)))
                        }
                        _ => {
                            return Err(error(&format!(
                                "confine: the rights of {path} must be \"r\" or \"rw\""
                            )))
                        }
                    }
                }
                self.host
                    .confine(&grants)
                    .map_err(|e| error(&format!("confine: {}", e.message())))?;
                let r = self.call_fn(&a[1], vec![]);
                self.host.unconfine();
                r
            }
            "help" => {
                let t = self.help.clone();
                self.out(t.as_bytes())?;
                Ok(Val::Null)
            }
            "halt" => {
                self.host.halt();
                Err(Flow::Exit(1))
            }
            "pwd" => Ok(Val::string(self.host.cwd())),
            "lines" => {
                arity(name, a, 0, 0)?;
                Ok(Val::list(self.stdin_lines()?))
            }
            "lines-of" => {
                arity(name, a, 1, 1)?;
                let buf = Rc::new(RefCell::new(Vec::new()));
                let saved = core::mem::replace(&mut self.io.stdout, Sink::Mem(buf.clone()));
                let r = self.call_fn(&a[0], vec![]);
                self.io.stdout = saved;
                r?;
                let text = String::from_utf8(core::mem::take(&mut *buf.borrow_mut()))
                    .map_err(|_| error("output is not valid UTF-8"))?;
                Ok(split_lines_val(&text))
            }
            "import" => {
                arity(name, a, 1, 1)?;
                let path = string(name, &a[0])?;
                self.import(path)
            }
            "succeeds" | "run" => Err(error(&format!(
                "{name} takes a command in parentheses: {name} (cmd)"
            ))),
            _ => Err(error(&format!("{name}: not implemented"))),
        }
    }

    /// JSON, CSV and TSV readers accept a string or a list of lines (a command's output).
    fn text_of_input(&self, v: &Val) -> Res<String> {
        match v {
            Val::Str(s) => Ok(String::from(&**s)),
            Val::List(l) => {
                let mut out = String::new();
                for (i, x) in l.iter().enumerate() {
                    if i > 0 {
                        out.push('\n');
                    }
                    match x {
                        Val::Str(s) => out.push_str(s),
                        x => {
                            return Err(error(&format!(
                                "expected lines of text, got a {} in the list",
                                x.kind()
                            )))
                        }
                    }
                }
                Ok(out)
            }
            v => Err(error(&format!(
                "expected text or lines, got a {}",
                v.kind()
            ))),
        }
    }

    fn list_op(&mut self, name: &str, args: Vec<Val>) -> Res<Val> {
        let a = &args;
        match name {
            "map" => {
                arity(name, a, 1, 2)?;
                let items = self.items(a, 1)?;
                let mut out = Vec::new();
                for x in items {
                    out.push(self.call_fn(&a[0], vec![x])?);
                }
                Ok(Val::list(out))
            }
            "filter" => {
                arity(name, a, 1, 2)?;
                let items = self.items(a, 1)?;
                let mut out = Vec::new();
                for x in items {
                    match self.call_fn(&a[0], vec![x.clone()])? {
                        Val::Bool(true) => out.push(x),
                        Val::Bool(false) => {}
                        v => {
                            return Err(error(&format!(
                                "filter needs a boolean, got a {}",
                                v.kind()
                            )))
                        }
                    }
                }
                Ok(Val::list(out))
            }
            "each" => {
                arity(name, a, 1, 2)?;
                let items = self.items(a, 1)?;
                for x in items {
                    self.call_fn(&a[0], vec![x])?;
                }
                Ok(Val::Null)
            }
            "fold" => {
                arity(name, a, 2, 3)?;
                let items = self.items(a, 2)?;
                let mut acc = a[0].clone();
                for x in items {
                    acc = self.call_fn(&a[1], vec![acc, x])?;
                }
                Ok(acc)
            }
            _ => {
                arity(name, a, 1, 2)?;
                let n = int(name, &a[0])?.max(0) as usize;
                let items = self.items(a, 1)?;
                Ok(Val::list(items.into_iter().take(n).collect()))
            }
        }
    }

    fn import(&mut self, path: &str) -> Res<Val> {
        if let Some(v) = self.imports.get(path) {
            return Ok(v.clone());
        }
        if self.importing.iter().any(|p| p == path) {
            return Err(error(&format!("import cycle: {path}")));
        }
        let bytes = self
            .host
            .read_file(path)
            .map_err(|e| error(&format!("import {path}: {}", e.message())))?;
        let src = String::from_utf8(bytes)
            .map_err(|_| error(&format!("import {path}: not valid UTF-8")))?;
        let stmts =
            crate::parser::parse(&src).map_err(|e| error(&format!("import {path}: {e}")))?;
        self.importing.push(String::from(path));
        let prefix: Rc<str> = Rc::from(path);
        let saved_module = self.module.replace(prefix.clone());
        let r = self.exec_block(&stmts, Mode::Stmt);
        self.module = saved_module;
        self.importing.pop();
        r?;
        let mut m = VMap::new();
        let pre = format!("{prefix}::");
        for (k, c) in self.fns.iter() {
            if let Some(n) = k.strip_prefix(&pre) {
                m.set(n, Val::Closure(c.clone()));
            }
        }
        let v = Val::map(m);
        self.imports.insert(String::from(path), v.clone());
        Ok(v)
    }
}

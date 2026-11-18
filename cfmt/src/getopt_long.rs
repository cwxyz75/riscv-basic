//! GNU `getopt_long` as a pure state machine. Unlike POSIX `getopt` (`getopt.rs`) it **permutes** the argument vector: an option
//! that follows operands is moved in front of them, so that on return `optind` is the first operand. A leading `+` in the option
//! string, or `POSIXLY_CORRECT` (the caller says: `posixly`), stops at the first operand instead. Long options may be abbreviated
//! to any unambiguous prefix; `--name=value` and `--name value` both give a required argument; an optional argument must be
//! attached. A leading `:` (after `+`) makes a missing argument return `:` and silences the messages (the caller's job).
//!
//! The vector is `&mut [&[u8]]`: the library layer builds it from the C strings and writes the permuted pointers back.
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy)]
pub struct LongOpt<'a> {
    pub name: &'a [u8],
    /// 0 `no_argument`, 1 `required_argument`, 2 `optional_argument`.
    pub has_arg: u8,
    /// The option has a `flag` pointer: the library stores `val` there and `next` returns 0.
    pub flag: bool,
    pub val: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// `-x`: not in the option string.
    Invalid(u8),
    /// `-x` needs an argument and there is none.
    MissingShort(u8),
    /// `--na` matches several options.
    Ambiguous(Vec<u8>),
    /// `--zz`: no such option.
    Unrecognized(Vec<u8>),
    /// `--name=value` for an option that takes none (the index into `longopts`).
    NoArgument(usize),
    /// `--name` needs an argument and there is none.
    MissingLong(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The option character or `val`, 0 for a flag option, `?`, `:`, or -1 at the end.
    pub ret: i32,
    /// The option's argument: (index into the permuted vector, byte offset in that word).
    pub optarg: Option<(usize, usize)>,
    /// `optopt` to set (for faults).
    pub optopt: Option<i32>,
    pub fault: Option<Fault>,
    /// The index into `longopts` of a long option that was recognised.
    pub longind: Option<usize>,
    /// For a flag option: the value to store through its `flag` pointer.
    pub flag_val: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetOptLong {
    pub optind: usize,
    /// Offset into a cluster of short options (0 between words), the word it is in and where the operands skipped before it begin.
    in_cluster: usize,
    cl_word: usize,
    cl_base: usize,
}

impl Default for GetOptLong {
    fn default() -> Self {
        Self::new()
    }
}

fn is_option_word(w: &[u8]) -> bool {
    w.len() > 1 && w[0] == b'-'
}

impl GetOptLong {
    pub const fn new() -> Self {
        GetOptLong {
            optind: 1,
            in_cluster: 0,
            cl_word: 0,
            cl_base: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// The next option. `argv[0]` is the program name (never looked at). See the module comment for the rules.
    pub fn next(
        &mut self,
        argv: &mut [&[u8]],
        optstring: &[u8],
        longopts: &[LongOpt],
        posixly: bool,
    ) -> Step {
        let argc = argv.len();
        let mut spec = optstring;
        let mut permute = !posixly;
        if let Some(rest) = spec.strip_prefix(b"+") {
            spec = rest;
            permute = false;
        } else if let Some(rest) = spec.strip_prefix(b"-") {
            // (`-` means "return operands as arguments of option 1": not supported, treated as plain)
            spec = rest;
        }
        let quiet = spec.first() == Some(&b':');
        let end = |ret: i32| Step {
            ret,
            optarg: None,
            optopt: None,
            fault: None,
            longind: None,
            flag_val: None,
        };
        if self.in_cluster == 0 {
            let base = self.optind;
            let mut j = base;
            if permute {
                while j < argc && !is_option_word(argv[j]) {
                    j += 1;
                }
            } else if j < argc && !is_option_word(argv[j]) {
                return end(-1);
            }
            if j >= argc {
                return end(-1);
            }
            if argv[j] == b"--" {
                argv[base..=j].rotate_left(j - base);
                self.optind = base + 1;
                return end(-1);
            }
            if argv[j].get(1) == Some(&b'-') {
                // a long option
                let word = argv[j];
                let body = &word[2..];
                let eq = body.iter().position(|&b| b == b'=');
                let name = &body[..eq.unwrap_or(body.len())];
                let mut found: Option<usize> = None;
                let mut ambiguous = false;
                for (i, lo) in longopts.iter().enumerate() {
                    if !lo.name.starts_with(name) {
                        continue;
                    }
                    if lo.name.len() == name.len() {
                        found = Some(i);
                        ambiguous = false;
                        break;
                    }
                    match found {
                        None => found = Some(i),
                        Some(f) => {
                            let o = &longopts[f];
                            if o.has_arg != lo.has_arg || o.flag != lo.flag || o.val != lo.val {
                                ambiguous = true;
                            }
                        }
                    }
                }
                let mut stop = j + 1;
                let mut st = end(-1);
                if ambiguous {
                    st.ret = i32::from(b'?');
                    st.optopt = Some(0);
                    st.fault = Some(Fault::Ambiguous(name.to_vec()));
                } else if let Some(f) = found {
                    let lo = longopts[f];
                    st.ret = lo.val;
                    if let Some(e) = eq {
                        if lo.has_arg == 0 {
                            st.ret = i32::from(b'?');
                            st.optopt = Some(lo.val);
                            st.fault = Some(Fault::NoArgument(f));
                        } else {
                            st.optarg = Some((usize::MAX, 2 + e + 1)); // (the index is filled in after the rotation)
                        }
                    } else if lo.has_arg == 1 {
                        if stop < argc {
                            st.optarg = Some((usize::MAX - 1, 0));
                            stop += 1;
                        } else {
                            st.ret = if quiet {
                                i32::from(b':')
                            } else {
                                i32::from(b'?')
                            };
                            st.optopt = Some(lo.val);
                            st.fault = Some(Fault::MissingLong(f));
                        }
                    }
                    if st.fault.is_none() {
                        st.longind = Some(f);
                        if lo.flag {
                            st.flag_val = Some(lo.val);
                            st.ret = 0;
                        }
                    }
                } else {
                    st.ret = i32::from(b'?');
                    st.optopt = Some(0);
                    st.fault = Some(Fault::Unrecognized(name.to_vec()));
                }
                argv[base..stop].rotate_left(j - base);
                self.optind = base + (stop - j);
                if let Some((i, off)) = st.optarg {
                    st.optarg = Some((if i == usize::MAX { base } else { base + 1 }, off));
                }
                return st;
            }
            self.in_cluster = 1;
            self.cl_word = j;
            self.cl_base = base;
        }
        // a short option inside the word at `cl_word`
        let word = argv[self.cl_word];
        let c = word[self.in_cluster];
        self.in_cluster += 1;
        let body = if quiet { &spec[1..] } else { spec };
        let found = body.iter().position(|&x| x == c && c != b':');
        let mut stop = self.cl_word + 1;
        let mut more = self.in_cluster < word.len();
        let mut st = end(i32::from(c));
        let mut optarg_at: Option<usize> = None; // `Some(offset)`: in the word; the separate word is marked below
        let mut separate = false;
        match found {
            None => {
                st.ret = i32::from(b'?');
                st.optopt = Some(i32::from(c));
                st.fault = Some(Fault::Invalid(c));
            }
            Some(f) if body.get(f + 1) == Some(&b':') => {
                if body.get(f + 2) == Some(&b':') {
                    // optional: only when attached
                    if more {
                        optarg_at = Some(self.in_cluster);
                        more = false;
                    }
                } else if more {
                    optarg_at = Some(self.in_cluster);
                    more = false;
                } else if stop < argc {
                    separate = true;
                    stop += 1;
                } else {
                    st.ret = if quiet {
                        i32::from(b':')
                    } else {
                        i32::from(b'?')
                    };
                    st.optopt = Some(i32::from(c));
                    st.fault = Some(Fault::MissingShort(c));
                }
            }
            Some(_) => {}
        }
        if more {
            return st; // the rest of the cluster is for the next call
        }
        let (base, wd) = (self.cl_base, self.cl_word);
        self.in_cluster = 0;
        argv[base..stop].rotate_left(wd - base);
        self.optind = base + (stop - wd);
        if let Some(off) = optarg_at {
            st.optarg = Some((base, off));
        } else if separate {
            st.optarg = Some((base + 1, 0));
        }
        st
    }
}

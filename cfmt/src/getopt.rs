//! POSIX `getopt` as a pure state machine over the argument vector: options end at the first argument that does not start with `-`
//! (no permutation), `--` ends them, and a leading `:` in `optstring` makes a missing argument return `:` instead of `?`. (A leading
//! `+`, the GNU spelling of "do not permute", is accepted and ignored.)

/// What went wrong with an option (the C layer prints the message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Invalid(u8),
    MissingArgument(u8),
}

/// One call's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The option character, `?`, `:`, or -1 at the end.
    pub ret: i32,
    /// The option's argument: (index into the argument vector, byte offset into that argument).
    pub optarg: Option<(usize, usize)>,
    /// Set for an invalid option or a missing argument.
    pub optopt: Option<u8>,
    pub fault: Option<Fault>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetOpt {
    /// The index of the next argument to look at (`optind`).
    pub optind: usize,
    nextchar: usize,
}

impl Default for GetOpt {
    fn default() -> Self {
        GetOpt::new()
    }
}

impl GetOpt {
    pub const fn new() -> Self {
        GetOpt {
            optind: 1,
            nextchar: 0,
        }
    }

    /// Start again from the first argument.
    pub fn reset(&mut self) {
        *self = GetOpt::default();
    }

    pub fn next(&mut self, args: &[&[u8]], optstring: &[u8]) -> Step {
        let end = |ret| Step {
            ret,
            optarg: None,
            optopt: None,
            fault: None,
        };
        if self.optind >= args.len() {
            return end(-1);
        }
        let arg = args[self.optind];
        let at = |i: usize| arg.get(i).copied().unwrap_or(0);
        if self.nextchar == 0 {
            if at(0) != b'-' || at(1) == 0 {
                return end(-1);
            }
            if at(1) == b'-' && at(2) == 0 {
                self.optind += 1;
                return end(-1);
            }
            self.nextchar = 1;
        }
        let c = at(self.nextchar);
        self.nextchar += 1;
        let spec = optstring.strip_prefix(b"+").unwrap_or(optstring);
        let quiet = spec.first() == Some(&b':');
        let spec = if quiet { &spec[1..] } else { spec };
        let found = spec.iter().position(|&x| x == c && c != b':');
        let at_end = at(self.nextchar) == 0;
        let Some(f) = found else {
            if at_end {
                self.optind += 1;
                self.nextchar = 0;
            }
            return Step {
                ret: i32::from(b'?'),
                optarg: None,
                optopt: Some(c),
                fault: Some(Fault::Invalid(c)),
            };
        };
        let mut optarg = None;
        if spec.get(f + 1) == Some(&b':') {
            // takes an argument: the rest of this word, or the next word
            if !at_end {
                optarg = Some((self.optind, self.nextchar));
                self.optind += 1;
            } else if self.optind + 1 < args.len() {
                optarg = Some((self.optind + 1, 0));
                self.optind += 2;
            } else {
                self.optind += 1;
                self.nextchar = 0;
                return Step {
                    ret: if quiet {
                        i32::from(b':')
                    } else {
                        i32::from(b'?')
                    },
                    optarg: None,
                    optopt: Some(c),
                    fault: Some(Fault::MissingArgument(c)),
                };
            }
            self.nextchar = 0;
        } else if at_end {
            self.optind += 1;
            self.nextchar = 0;
        }
        Step {
            ret: i32::from(c),
            optarg,
            optopt: None,
            fault: None,
        }
    }
}

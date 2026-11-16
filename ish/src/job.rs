//! Background jobs (SHELL-SPEC J1 to J10): the state `spawn` makes and `wait`, `wait-any`, `poll` and `kill` use.

use crate::host::{Pid, Stream};
use crate::value::Val;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

pub type MemBuf = Rc<RefCell<Vec<u8>>>;

#[derive(Debug)]
pub struct JobState {
    /// The program stages, in pipeline order, with their names.
    pub pids: Vec<(Pid, String)>,
    /// The status of each stage once it has been waited for.
    pub statuses: Vec<Option<i32>>,
    /// Read ends the shell still holds (captured or relayed output), and where they go (`None` discards).
    pub drains: Vec<(Stream, Option<MemBuf>)>,
    /// The captured output and error (when asked for), shown in the result.
    pub out: Option<MemBuf>,
    pub err: Option<MemBuf>,
    /// The result once the job has ended (the map `wait` returns).
    pub result: Option<Val>,
    /// Started with `{ "detach": true }`: not killed when the script ends.
    pub detach: bool,
    pub cap: usize,
}

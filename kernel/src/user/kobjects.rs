//! The kernel's own ends of channels and sockets (`KernelEnd`, `KernelSocket`), and the kernel's handle table.

use super::*;

/// The creator's end of a process's bootstrap channel, held by the kernel (S3c-1). Kernel
/// code (a test helper today, the launcher later) talks to the new process through it.
pub struct KernelEnd {
    pub(super) handle: Handle,
}

impl Drop for KernelEnd {
    /// Dropping an end closes it, so a forgotten end cannot leak a kernel-table slot.
    fn drop(&mut self) {
        with_state(|s| {
            let _ = s.ktable.close(&mut s.objs, self.handle);
        });
        notify();
    }
}

/// Take the kernel's end of `pid`'s bootstrap channel (once). Whoever takes it must `close` it.
pub fn take_bootstrap(pid: u64) -> Option<KernelEnd> {
    with_state(|s| {
        s.procs
            .get_mut(&pid)?
            .kernel_end
            .take()
            .map(|handle| KernelEnd { handle })
    })
}

impl KernelEnd {
    pub fn write(&self, data: &[u8]) -> Result<(), Status> {
        let r = with_state(|s| {
            ipc::channel_write_in(
                &mut s.objs,
                &mut s.ktable,
                self.handle,
                data,
                &[],
                kmem::jobs().jobs,
            )
        });
        notify();
        r
    }

    /// Take the oldest message (handles in it are closed: the kernel does not keep them).
    pub fn read(&self) -> Result<Vec<u8>, Status> {
        let r = with_state(|s| {
            let got = ipc::channel_read_in(
                &mut s.objs,
                &mut s.ktable,
                self.handle,
                MAX_MSG_BYTES,
                MAX_MSG_HANDLES,
                kmem::jobs().jobs,
            );
            s.objs.clear_dequeued_calls(); // (a kernel end's read lends nothing: L22)
            match got {
                Ok(msg) => {
                    for h in msg.handles {
                        let _ = s.ktable.close(&mut s.objs, h);
                    }
                    Ok(msg.data)
                }
                Err(ReadError::Status(st)) => Err(st),
                Err(ReadError::TooSmall { .. }) => Err(Status::BadState),
            }
        });
        notify();
        r
    }

    /// Send `data` together with a new VMO holding `image` (the receiver gets a VMO handle
    /// with the default VMO rights). Used to hand a program image to a launcher.
    pub fn write_with_vmo(&self, data: &[u8], image: &[u8]) -> Result<(), Status> {
        let r = with_state(|s| {
            let k = kmem::get();
            let pages = (image.len() as u64).div_ceil(4096).max(1);
            let id = k.vmos.create(k.jobs, pages);
            if k.vmos.write(k.mm, k.jobs, id, 0, image).is_err() {
                let _ = k.vmos.release(k.mm, k.jobs, id);
                return Err(Status::NoMemory);
            }
            let h = match ipc::vmo_object_create_in(
                &mut s.objs,
                &mut s.ktable,
                id,
                &mut ipc::Quota {
                    jobs: &mut *k.jobs,
                    job: Some(JobId::ROOT),
                },
            ) {
                Ok(h) => h,
                Err(e) => {
                    let _ = k.vmos.release(k.mm, k.jobs, id);
                    return Err(e);
                }
            };
            let r = ipc::channel_write_in(
                &mut s.objs,
                &mut s.ktable,
                self.handle,
                data,
                &[h],
                &mut *k.jobs,
            );
            if r.is_err() {
                let _ = s.ktable.close(&mut s.objs, h);
            }
            r
        });
        notify();
        r
    }

    /// Send `data` with handles that are in the kernel's table (`ktable_*` helpers make them);
    /// they move into the message. Nothing moves if the write fails.
    pub fn write_handles(&self, data: &[u8], handles: &[Handle]) -> Result<(), Status> {
        let r = with_state(|s| {
            ipc::channel_write_in(
                &mut s.objs,
                &mut s.ktable,
                self.handle,
                data,
                handles,
                kmem::jobs().jobs,
            )
        });
        notify();
        r
    }

    /// Wait until a message is readable or the peer has closed; false on timeout.
    pub fn wait_readable(&self, deadline: u64) -> bool {
        let h = self.handle;
        let me = thread::current();
        commit_wake();
        let r = thread::wait_deadline_registered(
            || {
                with_state(|s| {
                    ipc::signals_of(&s.objs, &s.ktable, h)
                        .map(|sig| sig & (abi::signals::READABLE | abi::signals::PEER_CLOSED) != 0)
                        .unwrap_or(true)
                })
            },
            || {
                with_state(|s| {
                    if let Ok(entry) = s.ktable.get(h) {
                        s.waits.register(
                            me,
                            entry.obj,
                            abi::signals::READABLE | abi::signals::PEER_CLOSED,
                        );
                    }
                })
            },
            deadline,
        );
        with_state(|s| s.waits.forget(me));
        r
    }

    /// Close the end (the same as dropping it).
    pub fn close(self) {
        drop(self);
    }
}

/// The kernel's end of a socket (in the kernel's own handle table), for the serial bridge.
#[cfg_attr(not(feature = "interactive"), allow(dead_code))]
pub struct KernelSocket {
    pub(super) handle: Handle,
}

impl Drop for KernelSocket {
    fn drop(&mut self) {
        with_state(|s| {
            let _ = s.ktable.close(&mut s.objs, self.handle);
        });
        notify();
    }
}

/// A new socket pair: the kernel keeps one end; the other end's handle is in the kernel's
/// table, ready to be sent to a process with [`KernelEnd::write_handles`].
#[cfg_attr(not(feature = "interactive"), allow(dead_code))]
pub fn kernel_socket_pair() -> Result<(KernelSocket, Handle), Status> {
    with_state(|s| {
        let (kernel, user) =
            ipc::socket_create_in(&mut s.objs, &mut s.ktable, &mut quota(JobId::ROOT))?;
        Ok((KernelSocket { handle: kernel }, user))
    })
}

/// A read-only VMO (READ, MAP, DUPLICATE, TRANSFER) holding `bytes`, as a handle in the
/// kernel's table.
/// Close a handle in the kernel's own table (cleanup when a send did not happen).
pub fn ktable_close(h: Handle) {
    with_state(|s| {
        let _ = s.ktable.close(&mut s.objs, h);
    });
    notify();
}

pub fn ktable_readonly_vmo(bytes: &[u8]) -> Result<Handle, Status> {
    with_state(|s| {
        let k = kmem::get();
        let pages = (bytes.len() as u64).div_ceil(4096).max(1);
        let id = k.vmos.create(k.jobs, pages);
        if k.vmos.write(k.mm, k.jobs, id, 0, bytes).is_err() {
            let _ = k.vmos.release(k.mm, k.jobs, id);
            return Err(Status::NoMemory);
        }
        let h = match ipc::vmo_object_create_in(
            &mut s.objs,
            &mut s.ktable,
            id,
            &mut ipc::Quota {
                jobs: &mut *k.jobs,
                job: Some(JobId::ROOT),
            },
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = k.vmos.release(k.mm, k.jobs, id);
                return Err(e);
            }
        };
        s.ktable
            .replace(
                &mut s.objs,
                h,
                Rights(Rights::READ.0 | Rights::MAP.0 | Rights::DUPLICATE.0 | Rights::TRANSFER.0),
            )
            .inspect_err(|_| {
                let _ = s.ktable.close(&mut s.objs, h);
            })
    })
}

#[cfg_attr(not(feature = "interactive"), allow(dead_code))]
impl KernelSocket {
    /// Up to `max` bytes the process wrote to its end.
    pub fn read(&self, max: usize) -> Result<Vec<u8>, Status> {
        let r = with_state(|s| {
            ipc::socket_read_in(&mut s.objs, &s.ktable, self.handle, max, kmem::jobs().jobs)
        });
        notify();
        r
    }

    /// Bytes for the process to read; returns how many were accepted.
    pub fn write(&self, data: &[u8]) -> Result<usize, Status> {
        let r = with_state(|s| {
            ipc::socket_write_in(&mut s.objs, &s.ktable, self.handle, data, kmem::jobs().jobs)
        });
        notify();
        r
    }

    /// The socket's signals (READABLE, PEER_CLOSED, ...), 0 if it names nothing.
    pub fn signals(&self) -> u32 {
        with_state(|s| ipc::signals_of(&s.objs, &s.ktable, self.handle).unwrap_or(0))
    }
}

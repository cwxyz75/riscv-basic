# Syscalls (OS track)

The contract between the kernel and userland, and the one kernel artifact meant to
be readable. **Empty skeleton, created in S0.** Syscalls are specified here *before*
they are implemented (S3); the target is about 50. Each gets an entry in this form,
and a test for every error case listed.

## Entry template

```text
### sys_<name>(args) -> result

What it does, in one or two sentences.

Preconditions:  what must hold about the arguments and the caller.
Rights:         the rights each handle argument must carry (or "none").
Effects:        what changes (objects created, handles moved, signals raised).
Blocking:       never | may block until <condition> | may be interrupted by <x>.
Errors:         BAD_HANDLE | WRONG_TYPE | ACCESS_DENIED | INVALID_ARGS | NO_MEMORY |
                (others as needed): when each is returned.
Invariants:     which rules in INVARIANTS.md this call touches.
Tests:          the test names covering success and every error above.
```

## Conventions (draft)

- Every syscall that names a kernel object takes a handle, never a pointer or an id.
- Every error is a distinct negative status code; there is no `errno` and no partial
  success without a documented return value.
- Pointers from user mode are validated by the kernel (inside the caller's own
  address space, correctly aligned, correct permission) before use.
- No syscall can make a handle with more rights than the one it was derived from
  (INVARIANTS.md H3).

## Index

(none yet)

#!/usr/bin/env python3
"""Headless boot test for the OS track (milestone S0).

Usage: os_boot_test.py OS_KERNEL_ELF [FAILING_OS_KERNEL_ELF [ROUND_ROBIN_OS_KERNEL_ELF]]

Boots the kernel in QEMU with no display and reads the structured lines it prints
on the serial port (OS-ARCHITECTURE.md, "Structured test output"):

    ##BOOT <name> <version>
    ##TEST <name> PASS
    ##TEST <name> FAIL <detail>
    ##PANIC <message>
    ##DONE pass=<n> fail=<n>

The kernel must report every test in REQUIRED, and it is booted with `-m 128M` (the
size it assumes, `abi::layout::RAM_SIZE`). The verdict comes from the kernel's exit status (QEMU's test finisher device:
0 for pass, nonzero for fail) AND the lines, which must agree with each other. If
a second kernel is given (built with its `fail-demo` feature, which makes one test
fail on purpose), this checks that failure is detected: nonzero exit status, a
FAIL line, and `fail=1` in the summary. A harness that cannot see a failure proves
nothing.

This parser is deliberately written separately from the Rust code that prints the
lines (`kcore::testlog`), so the two cannot share a misreading of the format.
"""
import re
import subprocess
import sys

TIMEOUT = 30

# Tests the kernel must report, by name. A kernel that silently drops a test must
# not pass (S1). Keep in step with `os-kernel/src/selftest.rs` and `main.rs`.
REQUIRED = [
    "bss_is_zeroed", "latch_works_on_the_target",
    "paging_enabled", "runs_in_the_upper_half", "low_half_is_empty",
    "kernel_mappings_are_w_xor_x", "kernel_sections_have_the_right_permissions",
    "direct_map_does_not_alias_the_image", "heap_works", "frame_database_is_sane",
    "lazy_commit_on_read", "lazy_commit_on_write", "unmapped_region_is_gone",
    "fault_outside_region_is_refused", "write_to_read_only_region_is_refused",
    "shared_vmo_is_one_memory", "churn_does_not_leak",
    # S2: threads, timer, scheduler
    "timer_ticks_advance", "threads_start_run_and_finish", "many_threads_do_not_leak",
    "sleep_wakes_on_time", "idle_runs_when_nothing_else_does",
    "wake_before_block_is_not_lost", "producer_consumer_loses_no_wakeup",
    "equal_priority_hogs_share_the_cpu", "two_waiters_on_one_queue_wake_in_order",
    "a_thread_can_spawn_threads", "joining_nothing_does_not_hang",
    "stack_guard_damage_is_detected", "fp_unit_is_off", "scheduler_class_is_fixed_priority",
    "higher_priority_runs_at_once_on_spawn", "lower_priority_waits_for_higher",
    "set_priority_takes_effect", "realtime_thread_stays_responsive_under_load",
    "the_latency_test_can_fail",
]

# Tests that only make sense for the fixed-priority class: a kernel booted with the
# round-robin class must pass everything else.
PRIORITY_ONLY = {
    "higher_priority_runs_at_once_on_spawn", "lower_priority_waits_for_higher",
    "set_priority_takes_effect", "realtime_thread_stays_responsive_under_load",
    "the_latency_test_can_fail",
}
TEST = re.compile(r"^##TEST (\S+) (PASS|FAIL)(?: (.*))?$")
DONE = re.compile(r"^##DONE pass=(\d+) fail=(\d+)$")
BOOT = re.compile(r"^##BOOT (\S+) (\S+)$")


def run(kernel: str, extra=()):
    """Boot `kernel`; return (lines, exit status, timed out)."""
    cmd = [
        "qemu-system-riscv64", "-machine", "virt", "-bios", "default",
        "-m", "128M", "-display", "none", "-serial", "stdio", "-monitor", "none",
        *extra, "-kernel", kernel,
    ]
    try:
        done = subprocess.run(cmd, stdin=subprocess.DEVNULL, capture_output=True, timeout=TIMEOUT)
        out, status, timed_out = done.stdout, done.returncode, False
    except subprocess.TimeoutExpired as e:
        out, status, timed_out = e.stdout or b"", None, True
    text = out.decode("utf-8", errors="replace").replace("\r\n", "\n")
    return [l for l in text.split("\n") if l.startswith("##")], status, timed_out, text


def parse(lines):
    """-> (boot, tests, done, panics, malformed): tests is a list of (name, ok, detail)."""
    boot, tests, done, panics, bad = None, [], None, [], []
    for line in lines:
        if m := BOOT.match(line):
            boot = m.groups()
        elif m := TEST.match(line):
            tests.append((m[1], m[2] == "PASS", m[3] or ""))
        elif m := DONE.match(line):
            done = (int(m[1]), int(m[2]))
        elif line.startswith("##PANIC "):
            panics.append(line[8:])
        else:
            bad.append(line)
    return boot, tests, done, panics, bad


def problems(lines, status, timed_out, expect_fail: bool, required=None):
    boot, tests, done, panics, bad = parse(lines)
    found = []
    if timed_out:
        found.append(f"timed out after {TIMEOUT} s")
    if boot is None:
        found.append("no ##BOOT line")
    if bad:
        found.append(f"malformed ## lines: {bad}")
    if panics:
        found.append(f"panic: {panics}")
    if not tests:
        found.append("no ##TEST lines")
    if done is None:
        found.append("no ##DONE line")
    else:
        passed = sum(1 for _, ok, _ in tests if ok)
        failed = len(tests) - passed
        if done != (passed, failed):
            found.append(f"##DONE {done} disagrees with the lines ({passed} pass, {failed} fail)")
        if expect_fail and failed != 1:
            found.append(f"expected exactly one failing test, saw {failed}")
        if not expect_fail and failed:
            found.append(f"failing tests: {[n for n, ok, _ in tests if not ok]}")
    missing = [n for n in (REQUIRED if required is None else required) if n not in {n for n, _, _ in tests}]
    if missing:
        found.append(f"required tests not reported: {missing}")
    if len({n for n, _, _ in tests}) != len(tests):
        found.append("duplicate test names")
    if not timed_out:
        if expect_fail and status == 0:
            found.append("a failing run exited with status 0")
        if not expect_fail and status != 0:
            found.append(f"exit status {status}, expected 0")
    return found


def main() -> int:
    if len(sys.argv) not in (2, 3, 4):
        print(__doc__, file=sys.stderr)
        return 2
    runs = [(sys.argv[1], False, None, "kernel")]
    if len(sys.argv) >= 3:
        runs.append((sys.argv[2], True, None, "fail-demo kernel (must be detected as failing)"))
    if len(sys.argv) == 4:
        generic = [n for n in REQUIRED if n not in PRIORITY_ONLY and n != "scheduler_class_is_fixed_priority"]
        generic.append("scheduler_class_is_round_robin")  # proves the feature really selected it
        runs.append((sys.argv[3], False, generic, "round-robin class kernel (generic tests only)"))
    failed = 0
    for kernel, expect_fail, required, label in runs:
        lines, status, timed_out, text = run(kernel)
        found = problems(lines, status, timed_out, expect_fail, required)
        if found:
            failed = 1
            print(f"os boot test FAILED for {label}:", *found, sep="\n  ", file=sys.stderr)
            print("--- serial output ---", text, sep="\n", file=sys.stderr)
        else:
            print(f"os boot test passed: {label} ({len(parse(lines)[1])} tests, exit status {status})")
    return failed


if __name__ == "__main__":
    sys.exit(main())

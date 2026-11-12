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

The machine type comes from $OS_MACHINE (default `virt`; `make os-boot-test-aia` uses
`virt,aia=aplic-imsic`); $OS_CPU, if set, is passed as `-cpu` (the target is RVA23: `rva23s64`, OD47). The kernel must report every test in REQUIRED, and it is booted with `-m 128M` (the
size it assumes, `abi::layout::RAM_SIZE`). The verdict comes from the kernel's exit status (QEMU's test finisher device:
0 for pass, nonzero for fail) AND the lines, which must agree with each other. If
a second kernel is given (built with its `fail-demo` feature, which makes one test
fail on purpose), this checks that failure is detected: nonzero exit status, a
FAIL line, and `fail=1` in the summary. A harness that cannot see a failure proves
nothing.

This parser is deliberately written separately from the Rust code that prints the
lines (`kcore::testlog`), so the two cannot share a misreading of the format.
"""
import os
import re
import subprocess
import sys

TIMEOUT = int(os.environ.get("OS_BOOT_TIMEOUT", "240"))

# Tests the kernel must report, by name. A kernel that silently drops a test must
# not pass (S1). Keep in step with `kernel/src/selftest.rs` and `main.rs`.
REQUIRED = [
    "bss_is_zeroed", "latch_works_on_the_target", "harts_come_online", "ipi_reaches_every_hart", "bkl_is_exclusive_under_contention", "a_wake_preempts_a_lower_priority_hart", "user_threads_run_in_parallel", "unmap_while_siblings_touch", "teardown_with_threads_on_other_harts", "kill_a_spinner_on_another_hart", "smp_churn_stress", "lock_order_violations_are_detected", "bkl_is_released_on_user_entry_and_idle",
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
    # S3b: user mode (the first group is reported by the user-level suite `init`, the second
    # by the kernel about processes)
    "user_mode_runs_and_prints", "clock_advances_in_user_mode", "unknown_syscalls_are_refused",
    "debug_write_pointer_rules", "handle_syscalls_reject_bad_values", "event_syscalls",
    "channel_round_trip", "channel_sends_handles", "channel_syscall_errors",
    "channel_buffer_too_small_keeps_message", "buffers_are_validated_before_a_read",
    "channel_txid_read", "channel_peer_closed", "user_cannot_forge_kernel_lines",
    "rights_cannot_be_widened", "stack_grows_lazily", "syscall_layer_bounds",
    "cycles_do_not_grow_memory",
    "user_programs_are_embedded", "init_runs_in_user_mode_and_passes",
    "process_teardown_returns_every_frame", "process_exit_releases_all_objects",
    "user_store_to_kernel_memory_is_fatal", "user_load_from_kernel_memory_is_fatal",
    "user_store_to_own_text_is_fatal", "user_jump_to_data_is_fatal",
    "user_privileged_instruction_is_fatal", "user_jump_to_unmapped_memory_is_fatal",
    "user_null_pointer_load_is_fatal", "user_exit_code_is_reported",
    "a_corrupt_program_is_refused_without_leaks",
    "registers_survive_a_syscall", "wide_arguments_are_not_truncated",
    "channel_read_checks_the_message_not_the_capacity", "user_jump_to_kernel_text_is_fatal",
    "a_syscall_with_sp_in_the_kernel_is_harmless", "the_stack_has_a_guard_page",
    "kernel_top_level_slots_are_stable", "two_processes_do_not_share_memory",
    "yielding_processes_do_not_share_memory",
    "two_process_teardown_returns_every_frame",
    "wait_arguments_are_checked", "wait_returns_at_once_when_the_condition_holds",
    "wait_with_deadline_zero_polls", "wait_times_out_at_the_deadline",
    "wait_is_woken_by_a_message", "channel_call_round_trip",
    "channel_call_skips_unrelated_messages",
    "channel_call_times_out_but_the_request_was_sent", "channel_call_buffer_too_small",
    "channel_call_argument_errors", "wait_and_call_argument_edges", "channel_call_carries_handles",
    "kernel_ends_survive_a_sweep", "wait_is_woken_by_the_peer_closing",
    "vmo_basics", "process_map_argument_checks", "process_start_argument_checks", "fetch_the_program_image", "child_exit_code_and_terminated_signal", "child_faults_are_contained", "parent_and_child_talk", "kill_a_spinning_child", "kill_a_blocked_child", "closing_the_last_process_handle_kills_the_child", "a_process_that_never_started_can_be_killed", "process_self_and_mapping_into_ourselves", "many_children_do_not_leak", "vmo_closed_while_mapped_keeps_its_pages", "thread_create_argument_checks", "threads_share_memory_and_report_exit_codes", "a_user_signal_wakes_a_blocked_thread", "thread_limit_and_slots_are_reused", "second_thread_outlives_the_first", "a_thread_can_end_the_whole_process", "a_fault_in_any_thread_ends_the_process", "the_last_thread_sets_the_exit_code", "many_threaded_children_do_not_leak", "threads_created_but_not_run_when_the_process_ends", "kill_a_process_that_is_creating_threads", "thread_create_with_a_full_handle_table", "a_thread_handle_closed_early_is_harmless", "a_thread_hog_cannot_exhaust_the_kernel", "socket_syscalls", "socket_end_of_stream", "socket_ends_travel_over_channels", "a_child_reads_a_socket_to_the_end", "a_blocked_reader_wakes_when_data_arrives", "a_blocked_writer_wakes_when_space_appears", "port_argument_checks", "port_wait_levels_timeouts_and_fairness", "port_wait_blocks_until_an_object_is_ready", "port_wait_sees_socket_data_and_object_wait_works_on_a_port", "kill_a_child_blocked_in_port_wait", "socket_memory_is_bounded", "kill_needs_the_kill_right_and_nothing_else", "fetch_the_hello_image", "runtime_startup_heap_and_stdio", "runtime_heap_stops_at_its_cap", "runtime_refuses_a_bad_startup_message", "runtime_without_a_startup_message", "runtime_io_fails_instead_of_spinning_without_wait", "launcher_spawns_a_program_from_the_boot_image", "the_process_handle_the_launcher_returns_cannot_manage", "the_launcher_refuses_bad_images_and_requests_without_leaks", "many_launcher_spawns_do_not_leak", "memfsd_serves_the_boot_image", "memfsd_tmp_is_the_only_writable_place", "memfsd_paths_cannot_escape_a_directory_handle", "memfsd_rights_only_shrink", "memfsd_protocol_violations", "memfsd_quotas_and_connection_limit", "object_info_names_objects_and_peers", "memfsd_refuses_endpoints_it_cannot_serve_and_orphans", "memfsd_refuses_both_ends_of_one_channel", "memfsd_survives_memory_pressure_from_quota_legal_clients", "memfsd_rights_are_checked_op_by_op", "memfsd_unlinked_open_files_live_until_closed", "memfsd_hands_out_programs_the_launcher_can_run", "memfsd_listing_pages_and_a_slow_client_stalls_only_itself", "memfsd_exits_when_every_client_is_gone", "syscalls_stay_fast_with_many_live_processes", "the_object_cap_is_reached_without_a_kernel_panic", "mappings_per_process_are_bounded", "queued_handles_are_charged_to_the_budget", "user_vector_instruction_is_fatal", "dead_port_bindings_vanish_at_once", "replacing_a_handle_ends_a_siblings_wait", "a_thread_killed_before_it_ran_runs_no_user_code", "kill_a_child_whose_threads_never_ran", "kill_a_child_with_blocked_and_spinning_threads", "closing_handles_early_and_kill_on_close", "a_thread_made_by_the_parent_faults_or_exits", "job_syscalls_check_handles_rights_and_arguments", "job_limits_stop_a_process_bomb_and_usage_comes_back", "job_limits_stop_a_thread_bomb", "job_memory_limits_stop_a_hog_and_are_credited_back", "a_child_job_cannot_exceed_its_parent_or_borrow_from_a_sibling", "job_kill_ends_every_process_below_and_forbids_more", "jobs_do_not_leak", "a_refused_process_start_changes_nothing_and_can_be_retried", "page_tables_are_charged_to_the_job", "a_job_limits_how_many_jobs_may_exist_below_it", "job_quotas_stop_an_object_and_an_ipc_hog_and_everything_comes_back", "job_create_sees_the_credit_of_a_job_closed_just_before", "threads_left_behind_when_init_exits", "no_kernel_thread_survives_init", "no_process_or_vmo_survives_init",
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
        "qemu-system-riscv64", "-machine", os.environ.get("OS_MACHINE", "virt"), "-bios", "default",
        "-m", "128M", "-display", "none", "-serial", "stdio", "-monitor", "none",
        *(["-cpu", os.environ["OS_CPU"]] if os.environ.get("OS_CPU") else []),
        # Harts (S5): the kernel must start and use all of them; OS_SMP=4 runs the whole suite on 4
        *(["-smp", os.environ["OS_SMP"]] if os.environ.get("OS_SMP") else []),
        # Deterministic guest time (instruction counting): the tests then do not depend on how fast
        # the host is (TIDY-UP T21). e.g. OS_ICOUNT=shift=4,sleep=off
        *(["-icount", os.environ["OS_ICOUNT"]] if os.environ.get("OS_ICOUNT") else []),
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


def round_robin_run(kernel):
    generic = [n for n in REQUIRED if n not in PRIORITY_ONLY and n != "scheduler_class_is_fixed_priority"]
    generic.append("scheduler_class_is_round_robin")  # proves the feature really selected it
    return (kernel, False, generic, "round-robin class kernel (generic tests only)")


def negative_control(kernel, test):
    """A kernel built with a safety mechanism switched off (the `fault-*` features) must fail the
    test that guards the mechanism: by that test failing, or (when the missing mechanism crashes
    or hangs the kernel) by the run not completing. A clean run means the guard is vacuous."""
    lines, status, timed_out, text = run(kernel)
    found = problems(lines, status, timed_out, False)
    if not found:
        print(f"negative control FAILED: {kernel} passed although it lacks the mechanism {test} guards",
              file=sys.stderr)
        return 1
    _, tests, _, panics, _ = parse(lines)
    failed = [n for n, ok, _ in tests if not ok]
    if test not in failed and not timed_out and not panics:
        print(f"negative control FAILED: expected {test} to fail, saw {failed} and {found}", file=sys.stderr)
        return 1
    how = "failed" if test in failed else "crashed or hung the kernel"
    print(f"negative control passed: {test} {how} without its mechanism ({kernel})")
    return 0


def main() -> int:
    # `--neg KERNEL TEST`: a fault-injection kernel that must be caught
    if len(sys.argv) == 4 and sys.argv[1] == "--neg":
        return negative_control(sys.argv[2], sys.argv[3])
    # `--rr KERNEL`: only the round-robin kernel (CI runs it as its own job)
    if len(sys.argv) == 3 and sys.argv[1] == "--rr":
        runs = [round_robin_run(sys.argv[2])]
    else:
        if len(sys.argv) not in (2, 3, 4):
            print(__doc__, file=sys.stderr)
            return 2
        runs = [(sys.argv[1], False, None, "kernel")]
        if len(sys.argv) >= 3:
            runs.append((sys.argv[2], True, None, "fail-demo kernel (must be detected as failing)"))
        if len(sys.argv) == 4:
            runs.append(round_robin_run(sys.argv[3]))
    failed = 0
    for kernel, expect_fail, required, label in runs:
        lines, status, timed_out, text = run(kernel)
        found = problems(lines, status, timed_out, expect_fail, required)
        want_harts = int(os.environ.get("OS_SMP", "1"))
        if want_harts > 1 and f"harts online: {want_harts}" not in text.split("\n"):
            found.append(f"the kernel did not report 'harts online: {want_harts}'")
        if found:
            failed = 1
            print(f"os boot test FAILED for {label}:", *found, sep="\n  ", file=sys.stderr)
            print("--- serial output ---", text, sep="\n", file=sys.stderr)
        else:
            print(f"os boot test passed: {label} ({len(parse(lines)[1])} tests, exit status {status})")
    return failed


if __name__ == "__main__":
    sys.exit(main())

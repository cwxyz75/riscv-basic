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

The machine type comes from $OS_MACHINE (default `virt,aia=aplic-imsic,iommu-sys=on`, the AIA and the platform IOMMU, which the kernel requires: plain
`virt` has only a PLIC and the kernel refuses to boot on it, PL6); the CPU is always `-cpu rva23s64` (the kernel refuses to boot below the profile, OD47, OD127); only `make os-boot-test-cpu-neg` sets $OS_CPU, to boot a CPU without the profile and expect the refusal. The kernel must report every test in REQUIRED, and it is booted with `-m 256M` (the
size the harness gives it: `OS_RAM`, default 256M, and `ram=` on the kernel line says which). The verdict comes from the kernel's exit status (QEMU's test finisher device:
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
import tempfile
import time
import sys

# The machine the OS track runs on (OD75): AIA (APLIC + IMSIC), required since PL6 (inter-hart interrupts are IMSIC messages).
DEFAULT_MACHINE = "virt,aia=aplic-imsic,iommu-sys=on"
# A boot is judged on progress, not on total time (T157, OD140): it fails when the guest prints nothing for STALL seconds (a hang),
# or when it runs past TIMEOUT (a ceiling far above any healthy run, a runaway). A loaded host (a CI runner, ten busy cores) slows
# a healthy suite several times over; a fixed total budget then reads that slowness as a failure.
TIMEOUT = int(os.environ.get("OS_BOOT_TIMEOUT", "1800"))
STALL = int(os.environ.get("OS_BOOT_STALL", "180"))
TIMEOUT_REASON = [f"timed out after {TIMEOUT} s"]

# Tests the kernel must report, by name. A kernel that silently drops a test must
# not pass (S1). Keep in step with `kernel/src/selftest.rs` and `main.rs`.
REQUIRED = [
    "bss_is_zeroed", "iommu_translation_matches_the_tables", "latch_works_on_the_target", "harts_come_online", "ipi_reaches_every_hart", "bkl_is_exclusive_under_contention", "a_wake_preempts_a_lower_priority_hart", "user_threads_run_in_parallel", "teardown_with_threads_on_other_harts", "unmap_while_siblings_touch", "kill_a_spinner_on_another_hart", "smp_churn_stress", "lock_order_violations_are_detected", "bkl_is_released_on_user_entry_and_idle", "idle_harts_take_no_big_lock_for_their_ticks",
    "paging_enabled", "runs_in_the_upper_half", "low_half_is_empty",
    "kernel_mappings_are_w_xor_x", "kernel_sections_have_the_right_permissions",
    "direct_map_does_not_alias_the_image", "ram_matches_the_machine", "the_console_is_where_the_boot_line_says", "the_image_is_where_the_loader_put_it", "kaslr_choice_follows_the_seed", "the_entropy_pool_is_seeded_as_the_tree_says", "heap_works", "frame_database_is_sane",
    "lazy_commit_on_read", "lazy_commit_on_write", "unmapped_region_is_gone",
    "fault_outside_region_is_refused", "write_to_read_only_region_is_refused",
    "shared_vmo_is_one_memory", "churn_does_not_leak",
    # S2: threads, timer, scheduler
    "timer_ticks_advance", "heap_grows_at_a_safe_point", "frame_cache_serves_and_accounts", "slab_serves_small_objects_and_everything_comes_back", "wfi_returns_with_interrupts_masked", "rt_threads_take_the_harts", "long_idle_suspends_and_a_short_sleeper_does_not", "pack_keeps_light_threads_on_few_harts", "harts_go_offline_and_come_back", "user_mode_ticks_take_no_big_lock", "user_clock_get_and_yield_take_no_big_lock", "threads_start_run_and_finish", "many_threads_do_not_leak",
    "sleep_wakes_on_time", "idle_runs_when_nothing_else_does",
    "wake_before_block_is_not_lost", "producer_consumer_loses_no_wakeup",
    "equal_priority_hogs_share_the_cpu", "two_waiters_on_one_queue_wake_in_order",
    "a_thread_can_spawn_threads", "joining_nothing_does_not_hang",
    "stack_guard_damage_is_detected", "the_stack_budget_check_trips_on_deep_recursion", "fp_unit_is_off", "scheduler_class_is_fixed_priority",
    "higher_priority_runs_at_once_on_spawn", "wake_many_makes_every_waiter_runnable_before_any_runs", "lower_priority_waits_for_higher",
    "set_priority_takes_effect", "realtime_thread_stays_responsive_under_load",
    "the_latency_test_can_fail",
    # S3b: user mode (the first group is reported by the user-level suite `init`, the second
    # by the kernel about processes)
    "user_mode_runs_and_prints", "clock_advances_in_user_mode", "unknown_syscalls_are_refused",
    "debug_write_pointer_rules", "debug_calls_need_a_debug_resource", "handle_syscalls_reject_bad_values", "a_process_holds_twenty_thousand_handles", "event_syscalls",
    "channel_round_trip", "channel_sends_handles", "channel_syscall_errors",
    "big_messages_are_copied_outside_the_object_lock",
    "hart_calls_need_the_resource_and_take_harts_offline_and_back",
    "system_suspend_is_gated_and_not_supported_without_the_firmware",
    "system_set_placement_needs_the_resource_and_changes_the_policy",
    "channel_buffer_too_small_keeps_message", "buffers_are_validated_before_a_read",
    "channel_txid_read", "channel_peer_closed", "user_cannot_forge_kernel_lines",
    "rights_cannot_be_widened", "stack_grows_lazily", "syscall_layer_bounds",
    "cycles_do_not_grow_memory",
    "user_programs_are_embedded", "init_runs_in_user_mode_and_passes",
    "process_teardown_returns_every_frame", "kernel_stack_budget", "process_exit_releases_all_objects",
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
    "vmo_basics", "process_map_argument_checks", "process_start_argument_checks", "fetch_the_program_image", "child_exit_code_and_terminated_signal", "programs_are_placed_at_random", "the_system_library_is_in_every_process", "child_faults_are_contained", "parent_and_child_talk", "kill_a_spinning_child", "kill_a_blocked_child", "closing_the_last_process_handle_kills_the_child", "a_process_that_never_started_can_be_killed", "process_self_and_mapping_into_ourselves", "many_children_do_not_leak", "vmo_closed_while_mapped_keeps_its_pages", "caller_stack_argument_checks", "threads_share_memory_and_report_exit_codes", "a_user_signal_wakes_a_blocked_thread", "thread_limit_and_slots_are_reused", "second_thread_outlives_the_first", "a_thread_can_end_the_whole_process", "a_fault_in_any_thread_ends_the_process", "the_last_thread_sets_the_exit_code", "many_threaded_children_do_not_leak", "threads_created_but_not_run_when_the_process_ends", "kill_a_process_that_is_creating_threads", "a_thread_handle_closed_early_is_harmless", "a_thread_hog_cannot_exhaust_the_kernel", "wake_baseline_bystanders_and_counters", "sending_a_waited_handle_ends_a_siblings_wait", "port_waiters_are_not_woken_by_unrelated_syscalls", "futex_sleepers_are_not_woken_by_unrelated_syscalls", "fp_state_is_private_to_each_thread_and_process", "vector_state_is_private_to_each_thread_and_process", "socket_syscalls", "socket_end_of_stream", "socket_half_close", "device_caps_need_the_resource", "port_oneshot_rearm_and_many_bindings", "benchmarks_run_and_report", "blk_round_trip", "blk_driver_restart", "c_transcript_matches_the_host", "lua_script_matches_the_host_build", "lua_upstream_calls", "lua_upstream_closure", "lua_upstream_constructs", "lua_upstream_events", "lua_upstream_goto", "lua_upstream_literals", "lua_upstream_locals", "lua_upstream_math", "lua_upstream_nextvar", "lua_upstream_pm", "lua_upstream_sort", "lua_upstream_strings", "lua_upstream_utf8", "lua_upstream_vararg", "lua_upstream_bitwise", "lua_upstream_tpack", "lua_upstream_coroutine", "lua_upstream_errors", "lua_upstream_db", "lua_upstream_gc", "lua_upstream_gengc", "lua_upstream_cstack", "lua_upstream_attrib", "lua_upstream_files", "futex_argument_errors", "several_waiters_on_one_port_are_woken_in_turn", "a_binding_ends_with_the_handle_it_was_made_through", "a_call_lends_its_priority_to_the_server", "pi_futex_bounds_inversion", "pi_futex_owner_death_poisons", "pi_futex_deadlock_is_refused", "pi_futex_argument_errors", "futex_times_out_not_early", "futex_wake_counts_and_fifo", "kill_a_futex_waiter", "mutex_excludes_across_harts", "condvar_producer_consumer", "once_and_semaphore", "backstops_follow_the_ram_of_the_machine", "reserved_abi_space_is_refused", "basic_golden_corpus_on_the_target", "devmgr_starts_blkd_for_the_disk", "fsd_keeps_files_across_a_restart", "clock_service_reports_the_rtc", "volmgr_serves_volumes_by_the_table", "devd_serves_null_and_zero", "symlinks_are_followed_by_the_client_not_the_server", "a_child_has_no_device_authority", "mmio_reads_the_virtio_magic", "interrupt_object_fires_and_masks", "device_interrupts_arrive_where_asked", "interrupt_identities_above_63_work", "interrupt_moves_do_not_lose_or_leak", "iommu_is_on_and_denies_by_default", "iommu_denies_messages_by_default", "iodomain_maps_exactly_what_was_granted", "iodomain_unmap_stops_the_device", "iodomain_faults_reach_the_supervisor", "iodomain_outlives_its_makers", "iodomain_calls_check_handles_rights_and_arguments", "socket_ends_travel_over_channels", "a_child_reads_a_socket_to_the_end", "a_blocked_reader_wakes_when_data_arrives", "a_blocked_writer_wakes_when_space_appears", "port_argument_checks", "port_wait_many_takes_a_batch_in_order", "port_wait_many_leaves_everything_on_a_bad_buffer", "port_wait_levels_timeouts_and_fairness", "port_wait_blocks_until_an_object_is_ready", "port_wait_sees_socket_data_and_object_wait_works_on_a_port", "kill_a_child_blocked_in_port_wait", "socket_memory_is_bounded", "kill_needs_the_kill_right_and_nothing_else", "fetch_the_hello_image", "fetch_the_conformance_image", "abi_conformance_passes", "runtime_startup_heap_and_stdio", "runtime_heap_stops_at_its_cap", "runtime_refuses_a_bad_startup_message", "runtime_without_a_startup_message", "runtime_io_fails_instead_of_spinning_without_wait", "launcher_spawns_a_program_from_the_boot_image", "the_process_handle_the_launcher_returns_cannot_manage", "the_launcher_refuses_bad_images_and_requests_without_leaks", "many_launcher_spawns_do_not_leak", "memfsd_serves_the_boot_image", "memfsd_tmp_is_the_only_writable_place", "memfsd_rename", "memfsd_paths_cannot_escape_a_directory_handle", "memfsd_rights_only_shrink", "memfsd_protocol_violations", "registry_publish_open_unavailable_and_views", "memfsd_accounts_confine_a_client", "memfsd_quotas_and_connection_limit", "object_info_names_objects_and_peers", "memfsd_refuses_endpoints_it_cannot_serve_and_orphans", "memfsd_refuses_both_ends_of_one_channel", "memfsd_survives_memory_pressure_from_quota_legal_clients", "memfsd_rights_are_checked_op_by_op", "memfsd_unlinked_open_files_live_until_closed", "memfsd_hands_out_programs_the_launcher_can_run", "memfsd_listing_pages_and_a_slow_client_stalls_only_itself", "memfsd_exits_when_every_client_is_gone", "syscalls_stay_fast_with_many_live_processes", "the_object_cap_is_reached_without_a_kernel_panic", "mappings_per_process_are_bounded", "queued_handles_are_charged_to_the_budget", "dead_port_bindings_vanish_at_once", "replacing_a_handle_ends_a_siblings_wait", "a_thread_killed_before_it_ran_runs_no_user_code", "kill_a_child_whose_threads_never_ran", "kill_a_child_with_blocked_and_spinning_threads", "closing_handles_early_and_kill_on_close", "kill_on_close_holds_after_the_child_took_a_self_handle", "a_thread_made_by_the_parent_faults_or_exits", "job_syscalls_check_handles_rights_and_arguments", "job_info_counts_the_cpu_time_its_threads_run", "a_runaway_rt_thread_does_not_hang_the_system", "cpu_time_is_counted_per_thread_and_process", "a_real_time_thread_stays_responsive_while_hogs_run_and_a_normal_one_does_not", "job_limits_stop_a_process_bomb_and_usage_comes_back", "job_limits_stop_a_thread_bomb", "job_memory_limits_stop_a_hog_and_are_credited_back", "a_two_gib_vmo_committed_whole_stays_within_the_latency_bound", "the_heap_gives_memory_back_when_it_is_freed", "runtime_heap_stops_at_its_cap_after_giving_memory_back", "a_decommit_under_way_refuses_the_calls_that_would_race_it", "a_decommit_the_job_cannot_pay_the_holes_for_is_no_memory", "a_two_gib_sparse_vmo_is_mapped_touched_and_given_back", "a_close_that_frees_a_chain_takes_interrupts_between_its_slices", "vmo_decommit_and_resize_from_user_mode", "copy_on_write_children_from_user_mode", "thread_suspend_stops_a_thread_and_its_registers_can_be_read_and_written", "a_stopped_thread_can_be_sent_to_a_handler_and_back", "a_thread_started_suspended_runs_only_after_resume_with_registers_written_first", "a_thread_started_suspended_and_resumed_at_once_runs", "suspend_and_state_calls_check_their_arguments_and_rights", "a_handler_thread_can_make_a_faulting_store_succeed_and_retry", "a_parent_handler_can_kill_a_child_that_took_an_illegal_instruction", "next_reaches_the_job_channel_and_closing_the_handle_is_next", "a_process_killed_with_a_pending_exception_leaks_nothing_and_a_closed_channel_is_skipped", "a_pager_serves_a_vmo_to_a_reader_in_another_process", "killing_a_reader_waiting_for_a_pager_leaks_nothing", "a_pager_that_dies_fails_the_reader_with_a_pager_fault", "a_pager_failure_is_a_pager_exception_and_a_supplied_page_lets_it_retry", "pager_calls_check_their_arguments", "fp_and_vector_state_round_trip_through_thread_state", "process_protect_and_range_unmap_from_user_mode", "a_write_to_a_protected_page_ends_the_process", "touching_past_a_shrunk_vmo_ends_the_process_cleanly", "a_confined_program_cannot_hold_much_uncharged_kernel_memory", "vmo_commit_charges_up_front_and_foreign_maps_need_it", "a_child_job_cannot_exceed_its_parent_or_borrow_from_a_sibling", "job_kill_ends_every_process_below_and_forbids_more", "jobs_do_not_leak", "dma_runs_are_allocated_and_freed_repeatedly", "a_refused_process_start_changes_nothing_and_can_be_retried", "thread_interrupt_argument_errors", "thread_interrupt_ends_an_object_wait", "thread_interrupt_ends_a_port_wait", "thread_interrupt_ends_a_futex_wait", "thread_interrupt_ends_a_channel_call", "thread_interrupt_before_the_wait_is_not_lost", "thread_interrupt_is_not_taken_by_a_poll", "thread_interrupt_waits_for_a_wait_that_would_block", "thread_interrupt_polls_leave_the_flag_on_every_wait", "thread_interrupt_is_taken_once", "thread_priority_is_bounded_by_the_job", "page_tables_are_charged_to_the_job", "thread_stacks_are_charged_to_the_job_and_credited_back", "thousands_of_threads_come_and_go", "stream_buffers_are_charged_to_the_client", "a_client_at_quota_stalls_only_itself", "one_socket_two_one_shot_bindings", "stream_pipe_numbers", "bystander_cost_of_sleeping_bindings", "a_dead_client_frees_everything", "a_bad_stream_handle_is_refused", "stream_budgets_with_the_launcher_defaults", "random_service_gives_seeded_distinct_bytes", "c_sockets_over_the_stack", "net_fetch_from_a_host_server", "net_httpd_for_the_host", "net_streams_over_loopback", "net_stream_quotas_and_bad_handles", "net_tcp_echo_for_the_host", "net_tcp_to_the_host_server", "net_tcp_churn", "net_streams_survive_a_stack_restart", "net_ping_round_trip_is_measured", "net_faults_are_survived", "net_clients_are_confined", "net_children_restart", "net_dhcp_configures_the_interface", "net_udp_echo_through_hostfwd", "net_static_config_and_ping_the_gateway", "net_driver_attaches_and_exchanges_a_frame", "a_process_ending_with_spawned_threads_leaks_nothing", "thread_spawn_starts_with_the_documented_state", "thread_spawn_error_cases", "thread_spawn_gives_every_stack_back", "thread_spawn_arena_is_bounded_and_reused", "two_thousand_threads_live", "ten_thousand_threads_leak_nothing", "a_job_limits_how_many_jobs_may_exist_below_it", "job_quotas_stop_an_object_and_an_ipc_hog_and_everything_comes_back", "job_create_sees_the_credit_of_a_job_closed_just_before", "threads_left_behind_when_init_exits", "no_kernel_thread_survives_init", "no_process_or_vmo_survives_init",
]

# Tests that only make sense for the fixed-priority class: a kernel booted with the
# round-robin class must pass everything else.
PRIORITY_ONLY = {
    "higher_priority_runs_at_once_on_spawn", "wake_many_makes_every_waiter_runnable_before_any_runs", "lower_priority_waits_for_higher",
    "set_priority_takes_effect", "realtime_thread_stays_responsive_under_load",
    "the_latency_test_can_fail",
    # (the controls need a starved low-priority thread: round-robin gives it a slice)
    "a_call_lends_its_priority_to_the_server", "pi_futex_bounds_inversion",
}
TEST = re.compile(r"^##TEST (\S+) (PASS|FAIL)(?: (.*))?$")
DONE = re.compile(r"^##DONE pass=(\d+) fail=(\d+)$")
BOOT = re.compile(r"^##BOOT (\S+) (\S+)$")
# `##BENCH <name> <decimal value> <unit>` (SCHED-SMP-SPEC 6.1, amends OD28): measurements, not verdicts. A
# well-formed line is kept for `OS_BENCH` (a TSV the run appends to); a malformed one fails the boot like any other
# malformed `##` line.
BENCH = re.compile(r"^##BENCH (\w+) (\d+) ([\w/]+)$")
LAST_BENCH = []
# `## iommu: fault cause=<decimal> dev=<bb:dd.f> iova=0x<hex> <read|write|exec|none>` (IOMMU-SPEC 7.2) and `## iommu: fence timeout dev=<bb:dd.f>`
# (6.4; `dev=none` for a fence of no one device): the IOMMU's console lines. Kept for the checks after the run; any other `## iommu:`
# line is malformed.
IOMMU_FAULT = re.compile(r"^## iommu: fault cause=(\d+) dev=([0-9a-f]{2}:[0-9a-f]{2}\.[0-7]) iova=0x([0-9a-f]+) (read|write|exec|none)$")
IOMMU_FENCE = re.compile(r"^## iommu: fence timeout dev=([0-9a-f]{2}:[0-9a-f]{2}\.[0-7]|none)$")
LAST_IOMMU_FAULTS = []
LAST_IOMMU_FENCES = []
# `kernel: iommu version=<major>.<minor> mode=<mode read back> caps=0x<hex>[ handover=<mode>]` (IOMMU-SPEC 2.5): the boot line
IOMMU_BOOT = re.compile(r"^kernel: iommu version=(\d+)\.(\d+) mode=(\S+) caps=0x([0-9a-f]+)(?: handover=(\S+))?$", re.M)
# The bus address of the function the harness puts behind the IOMMU (`-device virtio-rng-pci,...,addr=0x4`, in `run`)
IOMMU_TEST_DEV = "00:04.0"
# `##FF <name>=<value> ...` (T189): what the kernel understood of an `ff=` boot argument (test builds), so a run meant to start counters near their end cannot pass on a typo
LAST_FF = []


LAST_DISK = [None]
LAST_DISK2 = [None]


def check_disk(path):
    """The independent oracle for the block-driver tests (T44): after QEMU exits, look at the image
    itself. Expected (see `blk_round_trip` / `blk_driver_restart` in init/src/bin/suite.rs): sector 0
    untouched; sectors 100..227, 4000..4063, 7000..7031 and the last sector hold pattern(s, i) =
    (s*31 + i*7 + 5) mod 256; sectors 5000..5015 hold zeros or the in-flight write's data
    ((j*13 + 1) mod 256 at byte j of the 8 KiB); every other sector is zero. Returns problems."""
    SECTORS = 16 * 1024 * 2
    try:
        data = open(path, "rb").read()
    except OSError as e:
        return [f"the scratch disk is unreadable: {e}"]
    if len(data) != SECTORS * 512:
        return [f"the scratch disk changed size: {len(data)}"]
    found = []
    pat = lambda s: bytes((s * 31 + i * 7 + 5) & 0xFF for i in range(512))
    zero = bytes(512)
    for s in range(SECTORS):
        got = data[s * 512:(s + 1) * 512]
        if s == 0:
            want = [bytes((i * 7 + 3) & 0xFF for i in range(512))]
        elif 100 <= s < 228 or 4000 <= s < 4064 or 7000 <= s < 7032 or s == SECTORS - 1:
            want = [pat(s)]
        elif 5000 <= s < 5016:
            j = (s - 5000) * 512
            want = [zero, bytes(((j + i) * 13 + 1) & 0xFF for i in range(512))]
        else:
            want = [zero]
        if got not in want:
            found.append(f"disk sector {s} holds unexpected data (first bytes {got[:4].hex()})")
            if len(found) >= 5:
                break
    return found


def c_transcript_problems(text):
    """Build `libc/tests/c/transcript.c` with the host's compiler and libc, run it, and compare its
    output with what the guest printed between the `s7:` markers (lines `c|...`). The host's libc is
    the oracle for ours."""
    import tempfile
    lines = text.split("\n")
    if "s7: c transcript begins" not in lines or "s7: c transcript ends" not in lines:
        return ["the C transcript test did not run to the end"]
    a, b = lines.index("s7: c transcript begins"), lines.index("s7: c transcript ends")
    got = [l[2:] for l in lines[a + 1:b] if l.startswith("c|")]
    src = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "libc", "tests", "c", "transcript.c")
    with tempfile.TemporaryDirectory() as d:
        exe = os.path.join(d, "transcript_host")
        try:
            # (run in the scratch directory: the program's files are relative to it)
            cc = subprocess.run(["cc", "-O1", "-pthread", "-o", exe, src, "-lm"], capture_output=True, text=True)
            if cc.returncode != 0:
                return ["could not build the host oracle: " + cc.stderr[-300:]]
            run_ = subprocess.run([exe], capture_output=True, text=True, timeout=30, cwd=d)
        except (OSError, subprocess.TimeoutExpired) as e:
            return [f"the host oracle for the C transcript did not run: {e}"]
    want = run_.stdout.split("\n")
    if want and want[-1] == "":
        want.pop()
    if got and got[-1] == "":
        got.pop()
    if got == want:
        return []
    for i, (g, w) in enumerate(zip(got, want)):
        if g != w:
            return [f"C transcript line {i + 1} differs: guest {g!r}, host {w!r}"]
    return [f"C transcript length differs: guest {len(got)} lines, host {len(want)}"]


def lua_transcript_problems(text):
    """Build Lua from `third_party/lua` with the host's compiler (same sources, same settings
    header) and run `tests/lua/s7.lua` with it; the guest's transcript (lines `lua|...` between the
    `s7: lua transcript` markers) must be byte-identical. Compared after escaping every byte outside
    printable ASCII as \\xNN on both sides (the console shows only printable ASCII)."""
    import glob
    import shutil
    import tempfile
    lines = text.split("\n")
    if "s7: lua transcript begins" not in lines or "s7: lua transcript ends" not in lines:
        return ["the Lua script test did not run to the end"]
    a, b = lines.index("s7: lua transcript begins"), lines.index("s7: lua transcript ends")
    got = [l[4:] for l in lines[a + 1:b] if l.startswith("lua|")]
    root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
    srcs = sorted(f for f in glob.glob(os.path.join(root, "third_party", "lua", "src", "*.c"))
                  if not f.endswith("luac.c"))
    conf = os.path.join(root, "third_party", "lua", "os_lua.h")
    script = os.path.join(root, "tests", "lua", "s7.lua")
    with tempfile.TemporaryDirectory() as d:
        exe = os.path.join(d, "lua_host")
        try:
            cc = subprocess.run(["cc", "-O1", "-w", "-include", conf, "-o", exe, *srcs, "-lm"],
                                capture_output=True, text=True)
            if cc.returncode != 0:
                return ["could not build the host Lua: " + cc.stderr[-300:]]
            # (run in the scratch directory: the script's files are relative to it)
            # (a short relative path: Lua abbreviates long chunk names in error messages, and the
            # script replaces its own name in the transcript)
            shutil.copy(script, os.path.join(d, "s7.lua"))
            run_ = subprocess.run([exe, "s7.lua"], capture_output=True, timeout=60, cwd=d)
        except (OSError, subprocess.TimeoutExpired) as e:
            return [f"the host Lua did not run: {e}"]
    if run_.returncode != 0:
        return ["the host Lua failed: " + run_.stderr.decode(errors="replace")[-300:]]
    def esc(line):  # the guest prints printable ASCII as is and everything else as \\xNN
        return "".join(chr(c) if 32 <= c < 127 else "\\x%02x" % c for c in line)[:2000 - 4]
    want = [esc(l) for l in run_.stdout.split(b"\n")]
    if want and want[-1] == "":
        want.pop()
    if got and got[-1] == "":
        got.pop()
    if got == want:
        return []
    for i, (g, w) in enumerate(zip(got, want)):
        if g != w:
            return [f"Lua transcript line {i + 1} differs: guest {g!r}, host {w!r}"]
    return [f"Lua transcript length differs: guest {len(got)} lines, host {len(want)}"]


def scratch_disk():
    """A 16 MiB raw image whose sector 0 holds a known pattern (byte i = (i * 7 + 3) mod 256), so a
    driver that "reads" zeroes is visible (S6-SPEC section 6). A fresh copy per boot."""
    import atexit, tempfile
    f = tempfile.NamedTemporaryFile(prefix="os-disk-", suffix=".img", delete=False)
    f.write(bytes((i * 7 + 3) & 0xFF for i in range(512)))
    f.truncate(16 * 1024 * 1024)
    f.close()
    atexit.register(lambda: os.path.exists(f.name) and os.unlink(f.name))
    LAST_DISK[0] = f.name
    return f.name


def second_disk():
    """A 1 MiB raw image with its own sector-0 pattern (byte i = (i * 11 + 5) mod 256): the device manager
    must tell it apart from the first disk (slice DM). A fresh copy per boot."""
    import atexit, tempfile
    f = tempfile.NamedTemporaryFile(prefix="os-disk2-", suffix=".img", delete=False)
    f.write(bytes((i * 11 + 5) & 0xFF for i in range(512)))
    f.truncate(1024 * 1024)
    f.close()
    atexit.register(lambda: os.path.exists(f.name) and os.unlink(f.name))
    LAST_DISK2[0] = f.name
    return f.name


def fsck_second_disk():
    """The second disk, looked at from the host once the guest is gone: the suite put a doofs volume on it (the disk
    filesystem server and the volume manager tests), and the host `fsck-doofs` must accept what is left, however the guest
    processes ended (FS-SPEC section 11, check 7)."""
    import subprocess
    here = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    tool = os.path.join(here, "target", "debug", "fsck-doofs")
    if subprocess.run(["cargo", "build", "-q", "-p", "doofs-tools"], cwd=here).returncode != 0:
        return ["could not build the host fsck (cargo build -p doofs-tools)"]
    r = subprocess.run([tool, LAST_DISK2[0]], capture_output=True, text=True)
    if r.returncode != 0:
        return [f"fsck-doofs rejects the second disk (exit {r.returncode}):\n{r.stdout}{r.stderr}"]
    return []


def run_profiled(cmd, kernel):
    """Like the plain run, but stamps every output line with the host time it arrived, and appends one
    row per test to $OS_PROFILE (TSV: label, test, host seconds the test took, guest ticks it took).
    The host time of a test is the gap between its `##TEST` line and the previous one (the first test
    is measured from the boot line). Slice TS step 0: used by `scripts/profile_tests.py`."""
    import threading
    import time
    label = os.environ.get("OS_PROFILE_LABEL") or (
        f"{kernel.split('/')[1] if kernel.startswith('target/') else kernel}"
        f"|smp{os.environ.get('OS_SMP', '1')}|{os.environ.get('OS_MACHINE', DEFAULT_MACHINE)}"
        f"|{os.environ.get('OS_CPU', 'rva23s64')}")
    start = time.monotonic()
    proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE)
    stamped = []
    HOST_NET.clear()
    client = []
    tcp_client = []
    http_client = []
    tcp_server = HostTcpServer(HOST_NET.append)
    web = HostWeb()

    def pump():
        for raw in proc.stdout:
            stamped.append((time.monotonic(), raw))
            if b"httpd: listening on" in raw and not http_client:
                tp = int(os.environ.get("OS_NET_LAST_TCP_HOSTPORT", "0"))
                th = threading.Thread(target=host_httpd_client, args=(tp, HOST_NET.append), daemon=True)
                http_client.append(th)
                th.start()
            if b"s8: udpecho ready" in raw and not client:
                port = int(os.environ.get("OS_NET_LAST_HOSTPORT", "0"))
                th = threading.Thread(target=host_udp_client, args=(port, HOST_NET.append), daemon=True)
                client.append(th)
                th.start()
            if b"s9: tcpecho ready" in raw and not tcp_client:
                tp = int(os.environ.get("OS_NET_LAST_TCP_HOSTPORT", "0"))
                th = threading.Thread(target=host_tcp_client, args=(tp, HOST_NET.append), daemon=True)
                tcp_client.append(th)
                th.start()

    t = threading.Thread(target=pump, daemon=True)
    t.start()
    try:
        status = proc.wait(timeout=TIMEOUT)
        timed_out = False
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        status, timed_out = None, True
    t.join(timeout=5)
    for c in client + tcp_client + http_client:
        c.join(timeout=60)
    tcp_server.close()
    web.close()
    total = time.monotonic() - start
    out = b"".join(raw for _, raw in stamped)
    prev, ticks, rows = start, {}, []
    for when, raw in stamped:
        line = raw.decode("utf-8", errors="replace").strip()
        if line.startswith("tt: "):
            _, name, n = line.split()
            ticks[name] = n
        elif m := TEST.match(line):
            rows.append((m[1], when - prev))
            prev = when
    with open(os.environ["OS_PROFILE"], "a") as f:
        f.write(f"#boot\t{label}\t{total:.1f}\n")
        for name, secs in rows:
            f.write(f"{label}\t{name}\t{secs:.2f}\t{ticks.get(name, '')}\n")
    return out, status, timed_out


HOST_NET = []  # lines the host-side network client produced during the last run

TCP_SERVER_PORT = 47802   # the host TCP server the guest connects to (10.0.2.2:47802 through slirp)


def tcp_pattern(seed, n, start=0):
    """The byte pattern the guest's `pattern(seed, i)` makes: ((i * 2654435761) + seed) >> 24, 32-bit arithmetic."""
    return bytes(((((i * 2654435761) & 0xffffffff) + seed) & 0xffffffff) >> 24 for i in range(start, start + n))


def host_tcp_client(tcp_port, emit):
    """S9 step 4c: the host's own TCP as an independent client of the guest's echo listener, through `hostfwd=tcp`. 200,000
    bytes go out while the echo comes back on another thread; the host shuts down its write side; the echo must be complete,
    byte for byte, and end with a clean end-of-file (not a reset)."""
    import socket, threading, time
    want = tcp_pattern(11, 200_000)
    got = bytearray()
    bad = []
    s = None
    for _ in range(50):
        try:
            s = socket.create_connection(("127.0.0.1", tcp_port), timeout=10)
            break
        except OSError:
            time.sleep(0.2)
    if s is None:
        emit("##HOSTNET tcp_echo FAIL cannot connect to the forwarded port")
        return

    def reader():
        s.settimeout(30)
        try:
            while True:
                d = s.recv(65536)
                if not d:
                    return
                got.extend(d)
        except OSError as e:
            bad.append("read: %s" % e)

    th = threading.Thread(target=reader, daemon=True)
    th.start()
    try:
        for i in range(0, len(want), 8192):
            s.sendall(want[i:i + 8192])
        s.shutdown(socket.SHUT_WR)
    except OSError as e:
        bad.append("write: %s" % e)
    th.join(timeout=40)
    s.close()
    if bytes(got) != want:
        bad.append("echoed %d of %d bytes%s" % (len(got), len(want), "" if bytes(got) == want[:len(got)] else " (and wrong)"))
    emit("##HOSTNET tcp_echo " + ("FAIL " + "; ".join(bad) if bad else "PASS"))


HTTP_SERVER_PORT = 47804   # the host HTTP server `fetch` talks to (10.0.2.2:47804)
DNS_SERVER_PORT = 47803    # the scripted DNS server (10.0.2.2:47803, UDP)
FILE21 = None


def file21():
    """The 256 KiB file the host HTTP server serves (and whose digest the guest's `fetch` must reproduce)."""
    global FILE21
    if FILE21 is None:
        FILE21 = tcp_pattern(21, 262144)
    return FILE21


class HostWeb:
    """The host side of the fetch tests (S9 step 7b): an HTTP/1.0 server on HTTP_SERVER_PORT and a scripted DNS server on
    DNS_SERVER_PORT (UDP). `files.test` is 10.0.2.2, `alias.test` is a CNAME to it, `slow.test` drops its first query (the
    resolver must retry), `nosuch.test` is NXDOMAIN."""

    def __init__(self):
        import socket, socketserver, threading, http.server
        self.stop = threading.Event()
        data = file21()

        class H(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.0"

            def do_GET(self):
                if self.path == "/file.bin":
                    self.send_response(200)
                    self.send_header("Content-Type", "application/octet-stream")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                else:
                    self.send_error(404)

            def log_message(self, *a):
                pass

        class S(socketserver.ThreadingTCPServer):
            allow_reuse_address = True
            daemon_threads = True

        self.http = None
        self.dns = None
        try:
            self.http = S(("127.0.0.1", HTTP_SERVER_PORT), H)
            threading.Thread(target=self.http.serve_forever, daemon=True).start()
        except OSError:
            self.http = None
        try:
            self.dns = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            self.dns.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            self.dns.bind(("127.0.0.1", DNS_SERVER_PORT))
            self.dns.settimeout(0.3)
            self.seen = {}
            threading.Thread(target=self.dns_loop, daemon=True).start()
        except OSError:
            self.dns = None

    def dns_loop(self):
        import socket, struct
        while not self.stop.is_set():
            try:
                q, who = self.dns.recvfrom(512)
            except socket.timeout:
                continue
            except OSError:
                return
            if len(q) < 17:
                continue
            ident = q[:2]
            i, labels = 12, []
            while i < len(q) and q[i]:
                labels.append(q[i + 1:i + 1 + q[i]])
                i += 1 + q[i]
            name = b".".join(labels).decode().lower()
            question = q[12:i + 5]
            self.seen[name] = self.seen.get(name, 0) + 1
            if name == "slow.test" and self.seen[name] == 1:
                continue  # drop the first query: the resolver retries after a second
            def a_rr(owner_ptr=b"\xc0\x0c"):
                return owner_ptr + struct.pack("!HHIH", 1, 1, 60, 4) + bytes([10, 0, 2, 2])
            if name in ("files.test", "slow.test"):
                resp = ident + b"\x81\x80" + struct.pack("!HHHH", 1, 1, 0, 0) + question + a_rr()
            elif name == "alias.test":
                target = b"\x05files\x04test\x00"
                cn = b"\xc0\x0c" + struct.pack("!HHIH", 5, 1, 60, len(target)) + target
                resp = ident + b"\x81\x80" + struct.pack("!HHHH", 1, 2, 0, 0) + question + cn + target + struct.pack("!HHIH", 1, 1, 60, 4) + bytes([10, 0, 2, 2])
            else:
                resp = ident + b"\x81\x83" + struct.pack("!HHHH", 1, 0, 0, 0) + question
            self.dns.sendto(resp, who)

    def close(self):
        self.stop.set()
        if self.http:
            self.http.shutdown()
            self.http.server_close()
        if self.dns:
            self.dns.close()


def host_httpd_client(tcp_port, emit):
    """S9 step 7b: the host's own HTTP client against the guest's `httpd` through hostfwd=tcp: 16 connections (a download whose
    digest is checked, the index, 404, HEAD, 405, three kinds of 400, 414, 431, 505, a hang-up in mid-head, and four downloads
    at once). Every answer is checked; the server must then quit on its own after the 16th."""
    import hashlib, socket, threading, time
    want_sha = hashlib.sha256(tcp_pattern(13, 200_000)).hexdigest()
    bad = []

    def raw(data, hangup=False, timeout=20):
        for _ in range(60):
            try:
                s = socket.create_connection(("127.0.0.1", tcp_port), timeout=timeout)
                break
            except OSError:
                time.sleep(0.25)
        else:
            return None, b""
        try:
            s.sendall(data)
            if hangup:
                s.close()
                return None, b""
            buf = b""
            while True:
                d = s.recv(65536)
                if not d:
                    break
                buf += d
            return buf.split(b"\r\n", 1)[0], buf
        except OSError as e:
            bad.append("socket error: %s" % e)
            return None, b""
        finally:
            s.close()

    def status(line):
        try:
            return int(line.split()[1])
        except Exception:
            return -1

    def get(path, extra=b"", method=b"GET", version=b"HTTP/1.1"):
        return raw(method + b" " + path + b" " + version + b"\r\nHost: x\r\n" + extra + b"\r\n")

    def check(name, cond):
        if not cond:
            bad.append(name)

    line, body = get(b"/file.bin")
    head, _, payload = body.partition(b"\r\n\r\n")
    check("GET /file.bin: 200 and the digest", status(line) == 200 and hashlib.sha256(payload).hexdigest() == want_sha
          and b"Content-Length: 200000" in head and b"Connection: close" in head)
    line, body = get(b"/")
    check("GET /: index.html", status(line) == 200 and body.endswith(b"<html>hello from the OS</html>") and b"text/html" in body)
    line, _ = get(b"/nosuch")
    check("404", status(line) == 404)
    line, body = get(b"/file.bin", method=b"HEAD")
    check("HEAD: head only", status(line) == 200 and body.endswith(b"\r\n\r\n") and b"Content-Length: 200000" in body)
    line, body = get(b"/", method=b"POST")
    check("405 with Allow", status(line) == 405 and b"Allow: GET, HEAD" in body)
    line, _ = get(b"/../etc/passwd")
    check("400 for ..", status(line) == 400)
    line, _ = get(b"/%2e%2e/x")
    check("400 for an encoded ..", status(line) == 400)
    line, _ = get(b"/", extra=b"Content-Length: 5\r\n")
    check("400 for a body", status(line) == 400)
    line, _ = get(b"/" + b"a" * 1100)
    check("414", status(line) == 414)
    line, _ = get(b"/", extra=b"".join(b"H%d: v\r\n" % i for i in range(40)))
    check("431", status(line) == 431)
    line, _ = get(b"/", version=b"HTTP/2.0")
    check("505", status(line) == 505)
    raw(b"GET / HT", hangup=True)
    results = []

    def dl():
        l, b = get(b"/file.bin")
        results.append(status(l) == 200 and hashlib.sha256(b.partition(b"\r\n\r\n")[2]).hexdigest() == want_sha)

    ts = [threading.Thread(target=dl) for _ in range(4)]
    for t in ts:
        t.start()
    for t in ts:
        t.join(timeout=60)
    check("four downloads at once", len(results) == 4 and all(results))
    emit("##HOSTNET httpd " + ("FAIL " + "; ".join(bad) if bad else "PASS"))


class HostTcpServer:
    """The host TCP server a guest test connects to (10.0.2.2:TCP_SERVER_PORT through slirp): it sends a greeting and 300,000
    bytes, shuts down its write side, and checks the 100,000 bytes the guest sends back. Reports once, if the guest came."""

    def __init__(self, emit):
        import socket, threading
        self.emit = emit
        self.stop = threading.Event()
        self.srv = socket.socket()
        self.srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            self.srv.bind(("127.0.0.1", TCP_SERVER_PORT))
            self.srv.listen(4)
            self.th = threading.Thread(target=self.run, daemon=True)
            self.th.start()
        except OSError as e:
            self.th = None
            emit("##HOSTNET tcp_server NOTRUN cannot bind %d: %s" % (TCP_SERVER_PORT, e))

    def run(self):
        import socket
        self.srv.settimeout(0.5)
        while not self.stop.is_set():
            try:
                c, _ = self.srv.accept()
            except OSError:
                continue
            bad = []
            try:
                c.settimeout(30)
                c.sendall(b"HELLO-FROM-HOST\n" + tcp_pattern(3, 300_000))
                got = bytearray()
                c.shutdown(socket.SHUT_WR)
                while True:
                    d = c.recv(65536)
                    if not d:
                        break
                    got.extend(d)
                if bytes(got) != tcp_pattern(4, 100_000):
                    bad.append("received %d of 100000 bytes%s" % (len(got), "" if bytes(got) == tcp_pattern(4, len(got)) else " (and wrong)"))
            except OSError as e:
                bad.append(str(e))
            c.close()
            self.emit("##HOSTNET tcp_server " + ("FAIL " + "; ".join(bad) if bad else "PASS"))
            return

    def close(self):
        self.stop.set()
        if self.th:
            self.th.join(timeout=3)
        self.srv.close()


def host_udp_client(hostport, emit):
    """Q4 (NET-SPEC 12.2): the host's own UDP stack as the independent client of the guest's echo program, through the
    hostfwd. Datagrams of 1, 100, 512 and 1472 bytes come back byte for byte from the same address and port; 200 in a row all
    come back; datagrams of 1473 and 3000 bytes are fragmented by slirp and the stack drops the fragments (nothing is echoed,
    the guest stays responsive: the next small datagram is still answered)."""
    import random, socket, time
    rng = random.Random(7)
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.settimeout(3)
    dst = ("127.0.0.1", hostport)
    bad = []

    def exchange(data, wait=3.0):
        s.settimeout(wait)
        s.sendto(data, dst)
        try:
            got, who = s.recvfrom(4096)
        except socket.timeout:
            return None, None
        return got, who

    for n in (1, 100, 512, 1472):
        data = bytes(rng.randrange(256) for _ in range(n))
        got, who = exchange(data)
        if got != data:
            bad.append(f"{n}-byte datagram not echoed intact (got {None if got is None else len(got)})")
        elif who != dst:
            bad.append(f"{n}-byte echo came from {who}, not {dst}")
    ok200 = 0
    for i in range(200):
        data = bytes(rng.randrange(256) for _ in range(40)) + i.to_bytes(2, "big")
        got, _ = exchange(data)
        ok200 += got == data
    if ok200 != 200:
        bad.append(f"only {ok200} of 200 datagrams in a row were echoed")
    for n in (1473, 3000):
        got, _ = exchange(bytes(rng.randrange(256) for _ in range(n)), wait=1.0)
        if got is not None:
            bad.append(f"a {n}-byte datagram (fragmented by slirp) was echoed: the stack must drop fragments")
    # Q8: flood a guest endpoint whose owner stops reading; it must report datagrams dropped, answer pings meanwhile (the guest
    # checks that) and say when it has drained
    s.sendto(b"##stall", dst)
    time.sleep(0.4)
    for i in range(3000):
        s.sendto(b"F" * 64 + i.to_bytes(2, "big"), dst)
    time.sleep(3.0)  # (the guest drains its queue meanwhile, so the tail is delivered, not dropped)
    s.sendto(b"##tail", dst)  # the next delivered datagram carries the count of those dropped
    s.settimeout(15)
    try:
        while True:
            got, _ = s.recvfrom(4096)
            if got == b"##drained":
                break
    except socket.timeout:
        bad.append("the flooded guest never said it had drained")
    s.settimeout(3)
    got, _ = exchange(b"still alive")
    if got != b"still alive":
        bad.append("the guest stopped answering after the fragments")
    got, _ = exchange(b"##stop")  # tells the guest's echo loop to finish
    emit("##HOSTNET udp_echo " + ("FAIL " + "; ".join(bad) if bad else "PASS"))


def run_streaming(cmd):
    """Run QEMU, collect its output, and (when the guest announces its UDP echo program) run the host client against it."""
    import threading
    import time
    HOST_NET.clear()
    proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE)
    chunks = []
    last_out = [time.monotonic()]
    client = []
    tcp_client = []
    http_client = []
    tcp_server = HostTcpServer(HOST_NET.append)
    web = HostWeb()

    def pump():
        for raw in proc.stdout:
            chunks.append(raw)
            last_out[0] = time.monotonic()
            if b"httpd: listening on" in raw and not http_client:
                tp = int(os.environ.get("OS_NET_LAST_TCP_HOSTPORT", "0"))
                t = threading.Thread(target=host_httpd_client, args=(tp, HOST_NET.append), daemon=True)
                http_client.append(t)
                t.start()
            if b"s8: udpecho ready" in raw and not client:
                port = int(os.environ.get("OS_NET_LAST_HOSTPORT", "0"))
                t = threading.Thread(target=host_udp_client, args=(port, HOST_NET.append), daemon=True)
                client.append(t)
                t.start()
            if b"s9: tcpecho ready" in raw and not tcp_client:
                tp = int(os.environ.get("OS_NET_LAST_TCP_HOSTPORT", "0"))
                t = threading.Thread(target=host_tcp_client, args=(tp, HOST_NET.append), daemon=True)
                tcp_client.append(t)
                t.start()

    t = threading.Thread(target=pump, daemon=True)
    t.start()
    started = time.monotonic()
    status, timed_out = None, False
    while True:
        try:
            status = proc.wait(timeout=1)
            break
        except subprocess.TimeoutExpired:
            now = time.monotonic()
            if now - started > TIMEOUT:
                TIMEOUT_REASON[0] = f"timed out after {TIMEOUT} s"
            elif now - last_out[0] > STALL:
                TIMEOUT_REASON[0] = f"stalled: no output for {STALL} s (the last line: {b''.join(chunks[-1:]).decode(errors='replace').strip()[:100]!r})"
            else:
                continue
            proc.kill()
            proc.wait()
            status, timed_out = None, True
            break
    t.join(timeout=5)
    for c in client + tcp_client + http_client:
        c.join(timeout=60)
    tcp_server.close()
    web.close()
    return b"".join(chunks), status, timed_out


# The smoke tier runs the suite as two boots that CI starts as parallel jobs (OS_SHARD=a | b; TESTING.md, OD121): the guest
# runs only its part when the kernel line says `shard=a` or `shard=b`. Shard b is the network tests (`net_*` and the C sockets test) and the three heaviest stress tests
# (the same rule is in init/src/bin/suite.rs). Without OS_SHARD the whole suite runs, as the soak tier does.
SHARD = os.environ.get("OS_SHARD", "")
if SHARD not in ("", "a", "b", "k"):
    sys.exit(f"OS_SHARD must be a, b or k, not {SHARD!r}")
if SHARD == "k":
    # the kernel-only boot (KT1, KERNEL-V1-SPEC 8): the Kernel tests of `init/ktests.txt` and the kernel's own, no disk, no network
    os.environ["OS_DISK"] = "0"
    os.environ["OS_NET"] = "0"


def _read_lines(path):
    with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", path)) as f:
        return [l.strip() for l in f if l.strip() and not l.startswith("#")]


# The Kernel tests of the suite (`init/ktests.txt`, which the guest reads too) and every test the suite defines.
KERNEL_TESTS = {l.split()[0] for l in _read_lines("init/ktests.txt")}
SLOW_TESTS = {l.split()[0] for l in _read_lines("init/ktests.txt") if len(l.split()) > 1 and l.split()[1] == "slow"}
KFAST = os.environ.get("OS_KFAST") == "1"
def _suite_tests():
    src = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "init", "src", "bin", "suite.rs")).read()
    body = src[src.index("let tests: &[Test] = &["):]
    body = body[:body.index("\n    ];")]
    return set(re.findall(r'\(\s*"([a-z0-9_]+)",\s*[a-z0-9_:]+\s*,?\s*\)', body))


SUITE_TESTS = _suite_tests()

# HD4 part 3 (`make os-ktest-release-content`): the kernel is built with the `release-suite` feature, which has the release content (no boot self-tests,
# no scheduler or SMP tests) and still runs the suite and `user_tests.rs`'s checks. So the kernel's own boot tests are not required of it: what stays is
# every test of the suite, what `user_tests.rs` reports, and the scheduler class line.
RELEASE_SUITE = os.environ.get("OS_RELEASE_SUITE") == "1"


def _user_tests_names():
    src = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "kernel", "src", "user_tests.rs")).read()
    return set(re.findall(r'"([a-z0-9_]+)"', src)) | {"scheduler_class_is_fixed_priority"}


RELEASE_SUITE_KEEP = SUITE_TESTS | _user_tests_names() if RELEASE_SUITE else set()


def is_system_test(name):
    """A test of the suite that a kernel-only boot must not run (it starts a server or needs a disk or the network)."""
    return name.startswith("lua_upstream_") or (name in SUITE_TESTS and name not in KERNEL_TESTS)


SHARD_B_STRESS = ["ten_thousand_threads_leak_nothing", "a_thread_hog_cannot_exhaust_the_kernel", "cycles_do_not_grow_memory"]


def is_shard_b_test(name):
    return name.startswith("net_") or name == "c_sockets_over_the_stack" or name in SHARD_B_STRESS


def is_net_test(name):
    return name.startswith("net_") or name == "c_sockets_over_the_stack"


def shard_required(required):
    """The required tests of this run: the whole list, or the part the shard runs (the kernel's own tests run in both)."""
    names = REQUIRED if required is None else required
    if RELEASE_SUITE:
        names = [n for n in names if n in RELEASE_SUITE_KEEP or n.startswith("lua_upstream_")]
    if SHARD == "a":
        return [n for n in names if not is_shard_b_test(n)]
    if SHARD == "b":
        return [n for n in names if is_shard_b_test(n)]
    if SHARD == "k":
        return [n for n in names if not is_system_test(n) and not (KFAST and n in SLOW_TESTS)]
    return names


def net_problems(text):
    """The host's view of the network tests (NET-SPEC Q4, Q5): the echo client's verdict and the capture's checksums."""
    found = []
    if SHARD in ("a", "k"):
        return found  # (no network device, and no network tests: shard b has them)
    lossy = bool(os.environ.get("OS_NET_LOSSY"))
    if os.environ.get("OS_NET") != "0" and not lossy and "s8: udpecho ready" not in text and "net tests skipped" not in text:
        found.append("the network tests did not run (no marker from the UDP echo test)")
    if os.environ.get("OS_NET") != "0" and ("s8: udpecho ready" in text or lossy) and "netd: tcp secret from rngd" not in text:
        found.append("the stack did not take its TCP secret from the random service (no 'netd: tcp secret from rngd')")
    if "s8: udpecho ready" in text or lossy:
        for l in HOST_NET:
            print(l.replace("##HOSTNET", "host net:", 1))
        res = [l for l in HOST_NET if l.startswith("##HOSTNET udp_echo")]
        if lossy:
            pass  # (the UDP test does not run in lossy mode)
        elif not res:
            found.append("the host UDP client reported nothing")
        elif not res[0].endswith("PASS"):
            found.append(res[0])
        if "s9: tcpecho ready" in text:
            r = [l for l in HOST_NET if l.startswith("##HOSTNET tcp_echo")]
            if not r:
                found.append("the host TCP client reported nothing")
            elif not r[0].endswith("PASS"):
                found.append(r[0])
        if "s9: httpd ready" in text:
            r = [l for l in HOST_NET if l.startswith("##HOSTNET httpd")]
            if not r:
                found.append("the host HTTP client reported nothing")
            elif not r[0].endswith("PASS"):
                found.append(r[0])
        if "s9: fetch|" in text:
            import hashlib
            want = hashlib.sha256(file21()).hexdigest()
            tags = {}
            for l in text.split("\n"):
                if "s9: fetch|" in l:
                    parts = l.strip().split("|", 2)
                    if len(parts) == 3:
                        tags[parts[1]] = parts[2]
            for tag in ("name", "cname", "retry", "addr"):
                line = tags.get(tag, "")
                if "status=200" not in line or "bytes=262144" not in line or ("sha256=" + want) not in line:
                    found.append("fetch %s: %r (wanted the host's digest %s)" % (tag, line, want[:12]))
            if "s9: fetch tests done" not in text:
                found.append("the fetch tests did not finish")
        if "s9: host-tcp-server done" in text:
            r = [l for l in HOST_NET if l.startswith("##HOSTNET tcp_server")]
            if not r:
                found.append("the host TCP server reported nothing")
            elif not r[0].endswith("PASS"):
                found.append(r[0])
        pcap = os.environ.get("OS_NET_PCAP_USED")
        if pcap and os.path.exists(pcap):
            r = subprocess.run([sys.executable, os.path.join(os.path.dirname(__file__), "net_pcap_check.py"), pcap], capture_output=True, text=True)
            print("host net: capture check:", (r.stdout + r.stderr).strip().splitlines()[-1] if (r.stdout + r.stderr).strip() else "?")
            if r.returncode != 0:
                found.append("the packet capture has wrong checksums: " + (r.stdout + r.stderr).strip()[:300])
    return found


def free_udp_port():
    import socket
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def free_tcp_port():
    import socket
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


NET_PCAP = os.environ.get("OS_NET_PCAP")


LOSSY_PROCS = []

# the tests that run in lossy mode (the guest's suite runs only these when the kernel line says `net=lossy`)
LOSSY_REQUIRED = ["net_fetch_from_a_host_server", "net_httpd_for_the_host", "net_tcp_echo_for_the_host"]


def lossy_start(udp_port, tcp_port):
    """S9-SPEC 3.1: slirp lives in a second QEMU with no machine; the lossy relay sits between it and the guest's QEMU.
    OS_NET_LOSSY is the seed; OS_NET_LOSS, OS_NET_DUP, OS_NET_REORDER (fractions) and OS_NET_DELAY (ms) shape the link.
    Returns the guest's -netdev value."""
    import time
    slirp_port, relay_port = free_tcp_port(), free_tcp_port()
    qemu = ["qemu-system-riscv64", "-machine", "none", "-nographic", "-nodefaults", "-monitor", "none", "-serial", "none",
            "-netdev", f"user,id=u0,net=10.0.2.0/24,host=10.0.2.2,dhcpstart=10.0.2.15,hostfwd=udp:127.0.0.1:{udp_port}-:7777,hostfwd=tcp:127.0.0.1:{tcp_port}-:7778",
            "-netdev", "hubport,id=h0,hubid=0,netdev=u0",
            "-netdev", f"socket,id=s0,listen=127.0.0.1:{slirp_port}",
            "-netdev", "hubport,id=h1,hubid=0,netdev=s0"]
    LOSSY_PROCS.append(subprocess.Popen(qemu, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
    for _ in range(100):  # wait for slirp's QEMU to listen
        try:
            socket_probe(slirp_port)
            break
        except OSError:
            time.sleep(0.1)
    env = os.environ
    relay = [sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)), "net_lossy_relay.py"),
             "--listen", str(relay_port), "--slirp", str(slirp_port), "--seed", env["OS_NET_LOSSY"],
             "--loss", env.get("OS_NET_LOSS", "0.02"), "--dup", env.get("OS_NET_DUP", "0.01"),
             "--reorder", env.get("OS_NET_REORDER", "0.05"), "--delay", env.get("OS_NET_DELAY", "5")]
    LOSSY_PROCS.append(subprocess.Popen(relay, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE))
    for _ in range(100):
        try:
            socket_probe(relay_port)
            break
        except OSError:
            time.sleep(0.1)
    return f"socket,id=n0,connect=127.0.0.1:{relay_port}"


def socket_probe(port):
    import socket
    # (the slirp QEMU and the relay each accept one connection, so probing by connecting would use it up: probe by binding)
    with socket.socket() as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 0)
        try:
            s.bind(("127.0.0.1", port))
        except OSError:
            return  # taken: someone is listening
        raise OSError("not listening yet")


def lossy_stop():
    stats = ""
    for p in LOSSY_PROCS:
        p.terminate()
        try:
            _, err = p.communicate(timeout=5)
            if err and b"lossy relay" in err:
                stats = err.decode().strip()
        except Exception:
            p.kill()
    LOSSY_PROCS.clear()
    if stats:
        print(stats)


def net_args():
    port = int(os.environ.get("OS_NET_HOSTPORT") or free_udp_port())
    pcap = os.environ.get("OS_NET_PCAP") or os.path.join(tempfile.gettempdir(), f"os-net-{os.getpid()}-{port}.pcap")
    # the TCP forward is ready for S9 (guest port 7778; nothing listens there in S8, so a connect would be reset)
    tcp_port = int(os.environ.get("OS_NET_TCP_HOSTPORT") or free_tcp_port())
    os.environ["OS_NET_LAST_HOSTPORT"] = str(port)
    os.environ["OS_NET_LAST_TCP_HOSTPORT"] = str(tcp_port)
    os.environ["OS_NET_PCAP_USED"] = pcap
    user = f"user,id=n0,net=10.0.2.0/24,host=10.0.2.2,dhcpstart=10.0.2.15,hostfwd=udp:127.0.0.1:{port}-:7777,hostfwd=tcp:127.0.0.1:{tcp_port}-:7778"
    if os.environ.get("OS_NET_LOSSY"):
        netdev = lossy_start(port, tcp_port)
    else:
        netdev = user
    return [
        "-global", "virtio-mmio.force-legacy=false",
        "-netdev", netdev,
        "-device", "virtio-net-device,netdev=n0,mac=52:54:00:12:34:56",
        # the entropy device the random service seeds from (S9 step 5)
        "-object", "rng-random,filename=/dev/urandom,id=rng0",
        "-device", "virtio-rng-device,rng=rng0",
        "-object", f"filter-dump,id=f0,netdev=n0,file={pcap}",
    ]


LAST_RECORD = [None]


def rr_args():
    """The QEMU arguments of record/replay mode (KT2), or []. Recording needs no disk and no network device (they would need
    block and network replay filters): run it with OS_SHARD=k, or OS_DISK=0 OS_NET=0. Record mode runs the harts in one TCG thread
    (deterministic), so a race that needs true parallel harts may not reproduce under it (TESTING.md)."""
    rec, rep = os.environ.get("OS_RECORD"), os.environ.get("OS_REPLAY")
    if not rec and not rep:
        return []
    if os.environ.get("OS_DISK") != "0" or os.environ.get("OS_NET") != "0":
        sys.exit("OS_RECORD / OS_REPLAY need a boot with no disk and no network: OS_SHARD=k (or OS_DISK=0 OS_NET=0)")
    shift = "shift=auto"
    if rep:
        LAST_RECORD[0] = None
        return ["-icount", f"{shift},rr=replay,rrfile={rep}", *(["-s", "-S"] if os.environ.get("OS_GDB") == "1" else [])]
    os.makedirs(rec, exist_ok=True)
    LAST_RECORD[0] = os.path.join(rec, f"boot-{os.getpid()}-{int(time.time())}.rr")
    return ["-icount", f"{shift},rr=record,rrfile={LAST_RECORD[0]}"]


def elf_load_address(kernel: str) -> int:
    """The physical address the kernel ELF asks to be loaded at: `p_paddr` of its first PT_LOAD segment. The kernel is told this (`load=`
    on its line) and must find itself there (PL4c); a build that ignores the address it was meant for (an earlier version of the
    load-address test did) is caught because this is read from the ELF, not from what the build was asked for."""
    import struct
    with open(kernel, "rb") as f:
        head = f.read(64)
        phoff, = struct.unpack_from("<Q", head, 0x20)
        phentsize, phnum = struct.unpack_from("<HH", head, 0x36)
        for i in range(phnum):
            f.seek(phoff + i * phentsize)
            ph = f.read(phentsize)
            ptype, = struct.unpack_from("<I", ph, 0)
            if ptype == 1:
                paddr, = struct.unpack_from("<Q", ph, 24)
                return paddr
    raise SystemExit(f"{kernel}: no PT_LOAD segment")


def run(kernel: str, extra=()):
    """Boot `kernel`; return (lines, exit status, timed out)."""
    if os.environ.get("OS_LOAD") and int(os.environ["OS_LOAD"], 16) != elf_load_address(kernel):
        raise SystemExit(f"{kernel} is linked to load at {elf_load_address(kernel):#x}, not at OS_LOAD={os.environ['OS_LOAD']}: the load-address build did not take")
    cmd = [
        "qemu-system-riscv64", "-machine", os.environ.get("OS_MACHINE", DEFAULT_MACHINE), "-bios", "default",
        "-m", os.environ.get("OS_RAM", "256M"), "-display", "none", "-serial", "stdio", "-monitor", "none",
        # The clock (FS-SPEC section 7, step C1): the RTC starts from a known date and follows the virtual clock, so the test can
        # tell the time it is given from a made-up one.
        "-rtc", "base=2030-06-15T12:00:00,clock=vm",
        # HD1: `OS_DTB=<file>` boots with that device tree (`scripts/entropy_boot_test.py` strips the seeds from QEMU's own)
        *(["-dtb", os.environ["OS_DTB"]] if os.environ.get("OS_DTB") else []),
        "-cpu", os.environ.get("OS_CPU", "rva23s64"),  # PL1: the profile CPU is the only test CPU (only the negative control overrides it)
        # Harts (S5): the kernel must start and use all of them; OS_SMP=4 runs the whole suite on 4
        *(["-smp", os.environ["OS_SMP"]] if os.environ.get("OS_SMP") else []),
        # IOMMU5: the platform IOMMU is on the machine (`iommu-sys=on` in DEFAULT_MACHINE) and this virtio-pci function sits behind
        # it at 00:04.0 (`iommu_platform=on`: its DMA and its MSI-X messages go through the IOMMU). Nothing drives it but the suite's
        # `iommu_*` tests, which program it by hand to show that a function with no domain can neither reach memory nor interrupt.
        *([] if os.environ.get("OS_PCI_TEST") == "0" else [
            "-device", "virtio-rng-pci,iommu_platform=on,disable-legacy=on,addr=0x4"]),
        # Deterministic guest time (instruction counting): the tests then do not depend on how fast
        # the host is (work item T21). e.g. OS_ICOUNT=shift=4,sleep=off
        *(["-icount", os.environ["OS_ICOUNT"]] if os.environ.get("OS_ICOUNT") and not rr_args() else []),
        # Record and replay (KT2, TESTING.md): `OS_RECORD=<dir>` boots under QEMU's record mode and the harness keeps the recording only
        # when the boot fails; `OS_REPLAY=<file>` replays one (add OS_GDB=1 to wait for gdb on :1234 first)
        *rr_args(),
        # The scratch disk (S6): a virtio block device on the modern transport. The kernel itself
        # never touches it; OS_DISK=0 leaves it off.
        *([] if os.environ.get("OS_DISK") == "0" else [
            "-global", "virtio-mmio.force-legacy=false",
            # (QEMU gives the first device the highest virtio-mmio slot and the device tree lists the
            # slots lowest first, so the second disk is attached first: the 16 MiB scratch disk is
            # then device 0 for the suite, and the small one is device 1)
            "-drive", f"file={second_disk()},if=none,format=raw,id=hd1",
            "-device", "virtio-blk-device,drive=hd1",
            "-drive", f"file={scratch_disk()},if=none,format=raw,id=hd0",
            "-device", "virtio-blk-device,drive=hd0"]),
        *(["-append", " ".join(a for a in ("bench=full" if os.environ.get("OS_BENCH_FULL") == "1" else "",
                                           "net=lossy" if os.environ.get("OS_NET_LOSSY") else "",
                                           f"shard={SHARD}" if SHARD else "",
                                           "kfast" if SHARD == "k" and KFAST else "",
                                           f"ram={os.environ.get('OS_RAM', '256M')}",
                                           f"load={elf_load_address(kernel):#x}",
                                           os.environ.get("OS_BOOT_ARGS", ""),
                                           f"chaos={os.environ['OS_CHAOS_SEED']}" if os.environ.get("OS_CHAOS_SEED") else "",
                                           f"chaospoints={os.environ['OS_CHAOS_POINTS']}" if os.environ.get("OS_CHAOS_POINTS") else "") if a)]
          ),
        # The network (S8, NET-SPEC 3.1): one virtio-net device on QEMU's user-mode network, with the addresses set
        # explicitly so no test depends on a default, a host-forwarded UDP port to the guest's 7777, and a packet
        # capture of everything the guest and slirp exchange. OS_NET=0 leaves it off; OS_NET_PCAP names the capture;
        # OS_NET_HOSTPORT names the forwarded host port (otherwise a free one is chosen).
        *([] if os.environ.get("OS_NET") == "0" or SHARD in ("a", "k") else net_args()),
        *extra, "-kernel", kernel,
    ]
    if os.environ.get("OS_PROFILE"):
        out, status, timed_out = run_profiled(cmd, kernel)
    else:
        try:
            out, status, timed_out = run_streaming(cmd)
        finally:
            lossy_stop()
    text = out.decode("utf-8", errors="replace").replace("\r\n", "\n")
    return [l for l in text.split("\n") if l.startswith("##")], status, timed_out, text


def parse(lines):
    """-> (boot, tests, done, panics, malformed): tests is a list of (name, ok, detail)."""
    boot, tests, done, panics, bad = None, [], None, [], []
    LAST_BENCH.clear()
    LAST_FF.clear()
    LAST_IOMMU_FAULTS.clear()
    LAST_IOMMU_FENCES.clear()
    for line in lines:
        if line.startswith("##FF "):
            LAST_FF.append(line)
        elif m := IOMMU_FAULT.match(line):
            LAST_IOMMU_FAULTS.append((int(m[1]), m[2], int(m[3], 16), m[4]))
        elif m := IOMMU_FENCE.match(line):
            LAST_IOMMU_FENCES.append(m[1])
        elif m := BOOT.match(line):
            boot = m.groups()
        elif m := TEST.match(line):
            tests.append((m[1], m[2] == "PASS", m[3] or ""))
        elif m := DONE.match(line):
            done = (int(m[1]), int(m[2]))
        elif m := BENCH.match(line):
            LAST_BENCH.append(m.groups())
        elif line.startswith("##PANIC "):
            panics.append(line[8:])
        else:
            bad.append(line)
    return boot, tests, done, panics, bad


def problems(lines, status, timed_out, expect_fail: bool, required=None):
    boot, tests, done, panics, bad = parse(lines)
    found = []
    if timed_out:
        found.append(TIMEOUT_REASON[0])
    if boot is None:
        found.append("no ##BOOT line")
    if bad:
        found.append(f"malformed ## lines: {bad}")
    if "ff=" in os.environ.get("OS_BOOT_ARGS", ""):
        # (T189) the run asked for counters near their end: the kernel must say it understood, and every name and value
        if not LAST_FF:
            found.append("the boot arguments carry ff= but the kernel reported no ##FF line (the counters did not start where the run asked)")
        elif any(" error " in l for l in LAST_FF):
            found.append(f"ff= was not understood: {LAST_FF}")
    if panics:
        found.append(f"panic: {panics}")
    if LAST_IOMMU_FENCES:
        # (IOMMU-SPEC 6.4, IO8) a fence that did not complete leaks what it would have released: never part of a passing run
        found.append(f"IOMMU fence timeouts: {LAST_IOMMU_FENCES}")
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
    missing = [n for n in shard_required(required) if n not in {n for n, _, _ in tests}]
    if SHARD == "a" and any(is_shard_b_test(n) for n, _, _ in tests):
        found.append("shard a ran tests that belong to shard b")
    if SHARD == "k" and KFAST and any(n in SLOW_TESTS for n, _, _ in tests):
        found.append("a kfast boot ran tests marked slow")
    if SHARD == "k" and any(is_system_test(n) for n, _, _ in tests):
        found.append(f"a kernel-only boot ran System tests: {[n for n, _, _ in tests if is_system_test(n)]}")
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
    passed = [n for n, ok, _ in tests if ok]
    # (a control may name several tests, comma separated: those that end a process through the same mechanism; the first of them that fails or hangs is the detection)
    names = test.split(",")
    if len(names) > 1:
        hit = [n for n in names if n in failed]
        if hit:
            print(f"negative control passed: {hit[0]} failed without its mechanism ({kernel})")
            return 0
        order = [n for n in REQUIRED if n not in {x for x, _, _ in tests}]
        if order and order[0] in names:
            print(f"negative control passed: {order[0]} crashed or hung the kernel without its mechanism ({kernel})")
            return 0
        test = names[-1]
    if test in passed:
        if timed_out or not any(l.startswith("##DONE") for l in lines):
            # a race: the guarded test got through this time, and the kernel without the mechanism then crashed or hung in the next test that tears a process
            # down (the normal kernel finishes the same suite, so the fault is what stopped it). The guard is not vacuous. (Soak, 2026-11-23: on a hosted runner
            # `teardown_with_threads_on_other_harts` passed and `kill_a_spinning_child` stalled.)
            print(f"negative control passed: {test} passed, but the run did not complete without its mechanism ({kernel}; {found[:1]})")
            return 0
        print(f"negative control FAILED: {test} passed without its mechanism ({found})", file=sys.stderr)
        return 1
    if test not in failed:
        # the kernel crashed or hung: it must have been inside the guarded test, i.e. the named
        # test is the first required test that was not reported (not an earlier, unrelated hang)
        order = [n for n in REQUIRED if n not in {x for x, _, _ in tests}]
        if not order or order[0] != test:
            print(f"negative control FAILED: the run stopped before {test} (first missing: "
                  f"{order[:1]}), so the control never reached it", file=sys.stderr)
            return 1
    how = "failed" if test in failed else "crashed or hung the kernel"
    print(f"negative control passed: {test} {how} without its mechanism ({kernel})")
    return 0


def expect_panic(kernel, needle):
    """A kernel built to fail in one specific way (the `stack-overflow-demo` feature) must stop with a
    `##PANIC` line containing `needle`, a nonzero exit status and no hang: the mechanism that reports the
    failure works (a kernel without it would pass its tests or hang silently)."""
    lines, status, timed_out, _ = run(kernel)
    _, _, _, panics, _ = parse(lines)
    if timed_out:
        print(f"expected panic FAILED: {kernel} hung instead of reporting {needle!r}", file=sys.stderr)
        return 1
    if not any(needle in p for p in panics) or status in (0, None):
        print(f"expected panic FAILED: no ##PANIC containing {needle!r} with a failing exit status "
              f"(panics {panics}, status {status})", file=sys.stderr)
        return 1
    print(f"expected panic seen: {needle!r} ({kernel})")
    return 0


def selftest_k() -> int:
    """The harness must fail a kernel-only boot that lacks a Kernel test or ran a System test (KT1 acceptance)."""
    global SHARD
    SHARD = "k"
    need = shard_required(None)
    assert need and all(not is_system_test(n) for n in need), "the required list of a k boot holds a System test"
    def lines_for(names):
        out = ["##BOOT kernel test"]
        out += [f"##TEST {n} PASS" for n in names]
        out.append(f"##DONE pass={len(names)} fail=0")
        return out
    ok = problems(lines_for(need), 0, False, False)
    missing_case = problems(lines_for(need[1:]), 0, False, False)
    extra_case = problems(lines_for(need + ["memfsd_rename"]), 0, False, False)
    results = {"a complete boot passes": not any("required" in f or "System" in f for f in ok),
               "a missing Kernel test is reported": any("required tests not reported" in f for f in missing_case),
               "a System test is reported": any("System tests" in f for f in extra_case)}
    for what, good in results.items():
        print(("ok: " if good else "FAIL: ") + what)
    return 0 if all(results.values()) else 1


def main() -> int:
    if len(sys.argv) == 2 and sys.argv[1] == "--selftest-k":
        return selftest_k()
    # `--expect-panic NEEDLE KERNEL`: a kernel that must stop with a panic containing NEEDLE
    if len(sys.argv) == 4 and sys.argv[1] == "--expect-panic":
        return expect_panic(sys.argv[3], sys.argv[2])
    # `--neg KERNEL TEST`: a fault-injection kernel that must be caught
    if len(sys.argv) == 4 and sys.argv[1] == "--neg":
        return negative_control(sys.argv[2], sys.argv[3])
    # `--rr KERNEL`: only the round-robin kernel (CI runs it as its own job)
    if len(sys.argv) == 3 and sys.argv[1] == "--rr":
        runs = [round_robin_run(sys.argv[2])]
    # `--fail-demo KERNEL`: only the fail-demo kernel, which must be detected as failing (CI runs it when the harness changed)
    elif len(sys.argv) == 3 and sys.argv[1] == "--fail-demo":
        runs = [(sys.argv[2], True, None, "fail-demo kernel (must be detected as failing)")]
    else:
        if len(sys.argv) not in (2, 3, 4):
            print(__doc__, file=sys.stderr)
            return 2
        runs = [(sys.argv[1], False, LOSSY_REQUIRED if os.environ.get("OS_NET_LOSSY") else None, "kernel")]
        if len(sys.argv) >= 3:
            runs.append((sys.argv[2], True, None, "fail-demo kernel (must be detected as failing)"))
        if len(sys.argv) == 4:
            runs.append(round_robin_run(sys.argv[3]))
    failed = 0
    for kernel, expect_fail, required, label in runs:
        lines, status, timed_out, text = run(kernel)
        found = problems(lines, status, timed_out, expect_fail, required)
        want = os.environ.get("OS_REQUIRE_LINE")
        if want and want not in text and not expect_fail:
            # (KT9) a run meant to exercise a build feature (the allocation-failure sweep) must show that it did, or a kernel built without it passes vacuously
            found.append(f"OS_REQUIRE_LINE: the output has no {want!r}")
        # a recording is kept only for a failing boot (the fail-demo kernel fails by design: it is the acceptance check)
        if LAST_RECORD[0] and os.path.exists(LAST_RECORD[0]):
            if found or expect_fail:
                print(f"recorded: {LAST_RECORD[0]}  (replay: OS_REPLAY={LAST_RECORD[0]} OS_SHARD=k python3 scripts/os_boot_test.py ...)", file=sys.stderr)
            else:
                os.remove(LAST_RECORD[0])
        if os.environ.get("OS_SHOW"):
            for l in text.split("\n"):
                if l.startswith(os.environ["OS_SHOW"]):
                    print(l)
        if os.environ.get("OS_BENCH") and not found:
            bench_label = f"smp{os.environ.get('OS_SMP', '1')}"
            with open(os.environ["OS_BENCH"], "a") as f:
                for name, value, unit in LAST_BENCH:
                    f.write(f"{bench_label}\t{name}\t{value}\t{unit}\n")
        want_harts = int(os.environ.get("OS_SMP", "1"))
        if want_harts > 1 and f"harts online: {want_harts}" not in text.split("\n"):
            found.append(f"the kernel did not report 'harts online: {want_harts}'")
        # (the lossy mode runs only the bulk TCP tests, and the net shard only the network tests: the checks for the other tests would fail by design)
        strict = not expect_fail and not os.environ.get("OS_NET_LOSSY") and SHARD != "b"
        # The kernel must have found the AIA (APLIC + IMSIC, OD75); otherwise the interrupt-object test could pass by skipping.
        want_ic = "interrupt controller: AIA"
        if strict and want_ic not in text.split("\n"):
            found.append(f"the kernel did not report '{want_ic}'")
        # IOMMU5 (IOMMU-SPEC 2.5, 7.2): the boot line names a directory mode (Off and Bare would also make a denial test pass or fail
        # for the wrong reason), and the IOMMU self-test was not a skip; when the suite's default-deny test ran, the console carries
        # its fault lines, naming the harness's function, a read and a write.
        if not expect_fail:
            m = IOMMU_BOOT.search(text)
            if not m:
                found.append("the kernel did not print its 'kernel: iommu version=... mode=... caps=0x...' line")
            elif m[3] not in ("1LVL", "2LVL", "3LVL"):
                found.append(f"the IOMMU's directory mode read back is {m[3]}, not a directory: {m[0]!r}")
            if "kernel: iommu self-test skipped" in text:
                found.append("the IOMMU self-test was skipped (the debug translation interface did not work): "
                             + next(l for l in text.split("\n") if "kernel: iommu self-test skipped" in l))
            _, tests_run, _, _, _ = parse(lines)
            if any(n == "iommu_is_on_and_denies_by_default" for n, _, _ in tests_run) and os.environ.get("OS_PCI_TEST") != "0":
                mine = [f for f in LAST_IOMMU_FAULTS if f[1] == IOMMU_TEST_DEV]
                for want in ("read", "write"):
                    if not any(f[3] == want and f[0] == 258 for f in mine):
                        found.append(f"no '## iommu: fault cause=258 dev={IOMMU_TEST_DEV} ... {want}' line "
                                     f"(the IOMMU's lines: {LAST_IOMMU_FAULTS[:8]})")
        # The interrupt-object test must have run its full path (a disk, an interrupt), not skipped.
        if (strict and os.environ.get("OS_DISK") != "0"
                and "s6: interrupt path exercised" not in text):
            found.append("the interrupt-object test did not reach the device interrupt")
        # The block-driver tests must have found the disk and run, with interrupts.
        if (strict and os.environ.get("OS_DISK") != "0"
                and "s6: blkd exercised (interrupts)" not in text):
            found.append("the block-driver tests did not run with interrupts")
        # On the AIA, device interrupts must have been taken, none on a hart they were not aimed at, and
        # (with several harts) some on a hart other than the boot hart: delivery goes to the hart that asked.
        if strict and os.environ.get("OS_DISK") != "0":
            m = re.search(r"s6: device interrupts taken (\d+), off the boot hart (\d+), misrouted (\d+)", text)
            if not m:
                found.append("the kernel did not report the device interrupt counts")
            else:
                taken, off, mis = map(int, m.groups())
                if taken == 0 or mis != 0 or (want_harts > 1 and off == 0):
                    found.append(f"AIA interrupt delivery: taken {taken}, off the boot hart {off}, "
                                 f"misrouted {mis} (harts {want_harts})")
        # The disk, looked at from the host once the guest is gone (only when the guest ran the
        # block-driver tests to the end; a timed-out or failing run leaves it half written).
        if (strict and not found and os.environ.get("OS_DISK") != "0"
                and LAST_DISK[0]):
            found += check_disk(LAST_DISK[0])
            if LAST_DISK2[0]:
                found += fsck_second_disk()
        # The device manager found the disk (or, with OS_DISK=0, found none and started nothing).
        if strict and SHARD != "k":  # (the servers these checks look for are System tests: a kernel-only boot has none)
            want_dm = ("s6: devmgr: no block device" if os.environ.get("OS_DISK") == "0"
                       else "s6: devmgr found the disk")
            if want_dm not in text:
                found.append(f"the device manager test did not report '{want_dm}'")
            if (os.environ.get("OS_DISK") != "0"
                    and "s6: devmgr served two block devices" not in text):
                found.append("the device manager did not serve two block devices at once")
            # the disk filesystem server (FS-SPEC 5c) must have run its full path, not skipped for want of a disk
            if (os.environ.get("OS_DISK") != "0"
                    and "fs: fsd kept its files across a restart" not in text):
                found.append("the disk filesystem server test did not run to its end")
            # the volume manager (FS-SPEC 5d, 5e): the junk disk was ignored and the table named the volume
            if os.environ.get("OS_DISK") != "0":
                for want in ("fs: volmgr served the volume by the table and after a restart",
                             "volmgr: device 0: not a doofs volume, ignored"):
                    if want not in text:
                        found.append(f"the volume manager test did not report {want!r}")
            # the wall-clock service (FS-SPEC step C1) must have seen both the RTC and the no-RTC case
            if "clock: the clock service reported the RTC and the Unset case" not in text:
                found.append("the clock service test did not run to its end")
        # The C library (S7c): the guest's transcript must equal the same C program run on the host.
        if not expect_fail:
            found += net_problems(text)
        if strict and SHARD != "k":
            found += c_transcript_problems(text)
            found += lua_transcript_problems(text)
        if found:
            failed = 1
            # (the log goes to a file too, for CI to keep as an artifact and for a person to read: KT2)
            try:
                os.makedirs("target/fail-logs", exist_ok=True)
                with open(f"target/fail-logs/{re.sub(r'[^a-z0-9]+', '-', label.lower())}-{int(time.time())}.log", "w") as lf:
                    lf.write("\n".join(found) + "\n--- serial output ---\n" + text)
            except OSError:
                pass
            print(f"os boot test FAILED for {label}:", *found, sep="\n  ", file=sys.stderr)
            print("--- serial output ---", text, sep="\n", file=sys.stderr)
        else:
            print(f"os boot test passed: {label} ({len(parse(lines)[1])} tests, exit status {status})")
            # where the time goes (guest milliseconds per test, from the suite's `tt:` lines): the smoke budget is watched from here
            tts = []
            for l in text.split("\n"):
                if l.startswith("tt: "):
                    parts = l.split()
                    if len(parts) == 3 and parts[2].isdigit():
                        tts.append((int(parts[2]), parts[1]))
            if tts:
                tts.sort(reverse=True)
                print(f"suite guest time: {sum(t for t, _ in tts) / 1000:.0f} s over {len(tts)} tests; slowest: "
                      + ", ".join(f"{n} {t / 1000:.1f} s" for t, n in tts[:6]))
    return failed


if __name__ == "__main__":
    sys.exit(main())

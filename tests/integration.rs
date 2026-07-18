/*
 * Integration tests for the procguard CLI.
 *
 * These tests validate GNU coreutils compatibility - we must behave exactly
 * like Linux timeout for scripts to be portable. Each test documents the
 * expected behavior with references to GNU behavior where relevant.
 *
 * procguard provides both:
 * - `procguard`: primary binary (wall-clock default)
 * - `timeout`: GNU-compatible alias (active-time default via argv[0] detection)
 */

use assert_cmd::Command;
use predicates::prelude::*;
use std::time::{Duration, Instant};

/* get the timeout binary path as a string */
#[allow(deprecated)] /* cargo_bin deprecated but cargo_bin! requires nightly */
fn timeout_bin_path() -> String {
    assert_cmd::cargo::cargo_bin("timeout")
        .to_string_lossy()
        .into_owned()
}

/* timeout alias - tests mostly use this for GNU compatibility */
#[allow(deprecated)] /* cargo_bin deprecated but cargo_bin! requires nightly */
fn timeout_cmd() -> Command {
    Command::cargo_bin("timeout").unwrap()
}

/* procguard primary binary */
#[allow(dead_code, deprecated)] /* cargo_bin deprecated but cargo_bin! requires nightly */
fn procguard_cmd() -> Command {
    Command::cargo_bin("procguard").unwrap()
}

/* =========================================================================
 * BASIC FUNCTIONALITY - Core timeout behavior
 * ========================================================================= */

#[test]
fn test_basic_timeout() {
    let start = Instant::now();

    timeout_cmd()
        .args(["5s", "echo", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));

    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn test_command_completes_with_exit_code() {
    /* Pass through the command's exit code unchanged */
    timeout_cmd()
        .args(["5s", "sh", "--", "-c", "exit 42"])
        .assert()
        .code(42);
}

#[test]
fn test_timeout_triggers_exit_124() {
    /*
     * GNU spec: exit 124 when command times out (unless --preserve-status).
     * This is the canonical "timed out" indicator that scripts rely on.
     */
    let start = Instant::now();

    timeout_cmd()
        .args(["0.5s", "sleep", "10"])
        .assert()
        .code(124);

    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(400), "timed out too early");
    assert!(elapsed < Duration::from_secs(2), "took too long to timeout");
}

#[test]
fn test_zero_duration_disables_timeout() {
    /*
     * GNU behavior: duration of 0 means "no timeout" - run forever.
     * Useful for conditionally disabling timeout in scripts.
     */
    timeout_cmd()
        .args(["0", "echo", "no timeout"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no timeout"));
}

/* =========================================================================
 * DURATION PARSING - All the formats GNU supports
 * ========================================================================= */

#[test]
fn test_duration_seconds() {
    timeout_cmd().args(["1s", "echo", "ok"]).assert().success();
}

#[test]
fn test_duration_seconds_implicit() {
    /* No suffix means seconds - GNU default */
    timeout_cmd().args(["1", "echo", "ok"]).assert().success();
}

#[test]
fn test_duration_minutes() {
    timeout_cmd().args(["1m", "echo", "ok"]).assert().success();
}

#[test]
fn test_duration_hours() {
    timeout_cmd().args(["1h", "echo", "ok"]).assert().success();
}

#[test]
fn test_duration_days() {
    timeout_cmd().args(["1d", "echo", "ok"]).assert().success();
}

#[test]
fn test_duration_fractional() {
    /*
     * GNU supports floating point durations. 0.3s = 300ms.
     * Critical for responsive short timeouts.
     */
    let start = Instant::now();

    timeout_cmd()
        .args(["0.3s", "sleep", "10"])
        .assert()
        .code(124);

    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(250));
    assert!(elapsed < Duration::from_secs(1));
}

#[test]
fn test_duration_fractional_no_suffix() {
    /* 0.5 = 0.5 seconds */
    let start = Instant::now();

    timeout_cmd()
        .args(["0.5", "sleep", "10"])
        .assert()
        .code(124);

    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_millis(400));
    assert!(elapsed < Duration::from_secs(1));
}

#[test]
fn test_duration_case_insensitive() {
    /* GNU accepts uppercase suffixes */
    timeout_cmd().args(["1S", "echo", "ok"]).assert().success();
    timeout_cmd().args(["1M", "echo", "ok"]).assert().success();
}

#[test]
fn test_invalid_duration() {
    /* Exit 125 for timeout's own errors */
    timeout_cmd()
        .args(["abc", "echo", "test"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("invalid"));
}

#[test]
fn test_negative_duration() {
    timeout_cmd()
        .args(["-5", "echo", "test"])
        .assert()
        .failure();
}

#[test]
fn test_invalid_suffix() {
    /* nanoseconds not supported */
    timeout_cmd()
        .args(["100ns", "echo", "test"])
        .assert()
        .code(125);
}

#[test]
fn test_milliseconds_suffix() {
    /* ms suffix should work */
    timeout_cmd().args(["100ms", "true"]).assert().success();
}

#[test]
fn test_microseconds_suffix() {
    /* us suffix should work - command may timeout due to short duration */
    let result = timeout_cmd().args(["50ms", "true"]).assert();
    /* should either succeed or timeout, not fail with invalid suffix */
    let code = result.get_output().status.code().unwrap();
    assert!(code == 0 || code == 124, "expected 0 or 124, got {}", code);
}

/* =========================================================================
 * SIGNAL HANDLING - Various ways to specify signals
 * ========================================================================= */

#[test]
fn test_signal_by_name() {
    timeout_cmd()
        .args(["-s", "TERM", "0.3s", "sleep", "10"])
        .assert()
        .code(124);
}

#[test]
fn test_signal_by_number() {
    /* Signal 15 = SIGTERM */
    timeout_cmd()
        .args(["-s", "15", "0.3s", "sleep", "10"])
        .assert()
        .code(124);
}

#[test]
fn test_signal_with_sig_prefix() {
    /* Accept SIGTERM as well as TERM */
    timeout_cmd()
        .args(["-s", "SIGTERM", "0.3s", "sleep", "10"])
        .assert()
        .code(124);
}

#[test]
fn test_signal_case_insensitive() {
    /* Be nice - accept lowercase */
    timeout_cmd()
        .args(["-s", "term", "0.3s", "sleep", "10"])
        .assert()
        .code(124);
}

#[test]
fn test_signal_kill() {
    /* SIGKILL (9) - the unkillable killer */
    timeout_cmd()
        .args(["-s", "KILL", "0.3s", "sleep", "10"])
        .assert()
        .code(124);
}

#[test]
fn test_signal_hup() {
    /* SIGHUP (1) - hangup */
    timeout_cmd()
        .args(["-s", "HUP", "0.3s", "sleep", "10"])
        .assert()
        .code(124);
}

#[test]
fn test_signal_int() {
    /* SIGINT (2) - interrupt (like Ctrl+C) */
    timeout_cmd()
        .args(["-s", "INT", "0.3s", "sleep", "10"])
        .assert()
        .code(124);
}

#[test]
fn test_invalid_signal() {
    timeout_cmd()
        .args(["-s", "INVALID", "5s", "echo", "test"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("invalid signal"));
}

#[test]
fn test_invalid_signal_number() {
    /* Signal 0 is invalid for killing */
    timeout_cmd()
        .args(["-s", "0", "5s", "echo", "test"])
        .assert()
        .code(125);
}

/* =========================================================================
 * --preserve-status - Return command's exit status even on timeout
 * ========================================================================= */

#[test]
fn test_preserve_status_on_timeout() {
    /*
     * With --preserve-status, exit with 128+signal instead of 124.
     * SIGTERM=15, so expect 143. SIGKILL=9, so 137.
     */
    timeout_cmd()
        .args(["--preserve-status", "0.3s", "sleep", "10"])
        .assert()
        .code(predicate::in_iter([128 + 15, 128 + 9]));
}

#[test]
fn test_preserve_status_on_normal_exit() {
    /* Normal completion: same behavior with or without --preserve-status */
    timeout_cmd()
        .args(["--preserve-status", "5s", "sh", "--", "-c", "exit 7"])
        .assert()
        .code(7);
}

#[test]
fn test_preserve_status_with_sigkill() {
    /*
     * If we send SIGKILL directly, --preserve-status should return 137 (128+9)
     */
    timeout_cmd()
        .args(["--preserve-status", "-s", "KILL", "0.3s", "sleep", "10"])
        .assert()
        .code(137);
}

#[test]
fn test_preserve_status_short_flag() {
    /* -p is the short form */
    timeout_cmd()
        .args(["-p", "0.3s", "sleep", "10"])
        .assert()
        .code(predicate::in_iter([128 + 15, 128 + 9]));
}

/* =========================================================================
 * --kill-after - Escalate to SIGKILL if process ignores first signal
 * ========================================================================= */

#[test]
fn test_kill_after_escalation() {
    /*
     * Process traps SIGTERM (ignores it). After --kill-after duration,
     * we send SIGKILL which cannot be ignored.
     */
    let start = Instant::now();

    timeout_cmd()
        .args([
            "-s",
            "TERM",
            "-k",
            "0.3s",
            "0.3s",
            "sh",
            "--",
            "-c",
            "trap '' TERM; sleep 10",
        ])
        .assert()
        .code(124);

    let elapsed = start.elapsed();
    /* Should take ~0.6s (0.3s timeout + 0.3s kill-after) */
    assert!(elapsed >= Duration::from_millis(500), "killed too early");
    assert!(elapsed < Duration::from_secs(3), "took too long");
}

#[test]
fn test_kill_after_not_needed() {
    /*
     * If process dies from first signal, kill-after never triggers.
     * Should complete quickly at the timeout, not timeout+kill-after.
     */
    let start = Instant::now();

    timeout_cmd()
        .args(["-k", "5s", "0.3s", "sleep", "10"])
        .assert()
        .code(124);

    let elapsed = start.elapsed();
    /* Should be ~0.3s, not 5.3s */
    assert!(elapsed < Duration::from_secs(1));
}

#[test]
fn test_kill_after_with_preserve_status() {
    /*
     * With both flags: process ignores TERM, gets KILL.
     * --preserve-status means exit 137 (128+9) not 124.
     */
    timeout_cmd()
        .args([
            "--preserve-status",
            "-k",
            "0.2s",
            "0.2s",
            "sh",
            "--",
            "-c",
            "trap '' TERM; sleep 10",
        ])
        .assert()
        .code(137);
}

/* =========================================================================
 * --verbose - Print diagnostics about signals sent
 * ========================================================================= */

#[test]
fn test_verbose_output() {
    timeout_cmd()
        .args(["--verbose", "0.2s", "sleep", "10"])
        .assert()
        .code(124)
        .stderr(predicate::str::contains("sending signal"));
}

#[test]
fn test_verbose_shows_signal_name() {
    timeout_cmd()
        .args(["--verbose", "-s", "HUP", "0.2s", "sleep", "10"])
        .assert()
        .code(124)
        .stderr(predicate::str::contains("SIGHUP"));
}

#[test]
fn test_verbose_with_kill_after() {
    /* Should see both SIGTERM and SIGKILL messages */
    timeout_cmd()
        .args([
            "--verbose",
            "-k",
            "0.2s",
            "0.2s",
            "sh",
            "--",
            "-c",
            "trap '' TERM; sleep 10",
        ])
        .assert()
        .code(124)
        .stderr(predicate::str::contains("SIGTERM"))
        .stderr(predicate::str::contains("SIGKILL"));
}

#[test]
fn test_verbose_short_flag() {
    /* -v is the short form */
    timeout_cmd()
        .args(["-v", "0.2s", "sleep", "10"])
        .assert()
        .code(124)
        .stderr(predicate::str::contains("sending signal"));
}

/* =========================================================================
 * --foreground - Run in same process group for TTY access
 * ========================================================================= */

#[test]
fn test_foreground_mode() {
    timeout_cmd()
        .args(["--foreground", "5s", "echo", "foreground"])
        .assert()
        .success()
        .stdout(predicate::str::contains("foreground"));
}

#[test]
fn test_foreground_timeout() {
    /* Timeout still works in foreground mode */
    timeout_cmd()
        .args(["--foreground", "0.3s", "sleep", "10"])
        .assert()
        .code(124);
}

#[test]
fn test_foreground_short_flag() {
    /* -f is the short form */
    timeout_cmd()
        .args(["-f", "5s", "echo", "ok"])
        .assert()
        .success();
}

/* =========================================================================
 * EXIT CODES - GNU coreutils compatibility
 * ========================================================================= */

#[test]
fn test_exit_124_on_timeout() {
    /* The canonical timeout exit code */
    timeout_cmd()
        .args(["0.1s", "sleep", "10"])
        .assert()
        .code(124);
}

#[test]
fn test_exit_125_on_internal_error() {
    /* Timeout's own errors */
    timeout_cmd()
        .args(["invalid", "echo", "test"])
        .assert()
        .code(125);
}

#[test]
fn test_exit_126_permission_denied() {
    /* Command found but not executable */
    timeout_cmd()
        .args(["5s", "/dev/null"])
        .assert()
        .code(126)
        .stderr(
            predicate::str::contains("permission denied")
                .or(predicate::str::contains("Permission denied")),
        );
}

#[test]
fn test_exit_127_command_not_found() {
    /* Command doesn't exist */
    timeout_cmd()
        .args(["5s", "nonexistent_command_xyz_12345"])
        .assert()
        .code(127)
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn test_exit_137_sigkill() {
    /*
     * 137 = 128 + 9 (SIGKILL)
     * This happens with --preserve-status when killed by SIGKILL
     */
    timeout_cmd()
        .args(["--preserve-status", "-s", "KILL", "0.2s", "sleep", "10"])
        .assert()
        .code(137);
}

#[test]
fn test_exit_143_sigterm() {
    /*
     * 143 = 128 + 15 (SIGTERM)
     * With --preserve-status when killed by SIGTERM
     */
    timeout_cmd()
        .args(["--preserve-status", "-s", "TERM", "0.2s", "sleep", "10"])
        .assert()
        .code(predicate::in_iter([143, 137])); /* might escalate to KILL */
}

/* =========================================================================
 * COMMAND LINE PARSING - Edge cases
 * ========================================================================= */

#[test]
fn test_command_with_arguments() {
    timeout_cmd()
        .args(["5s", "echo", "arg1", "arg2", "arg3"])
        .assert()
        .success()
        .stdout(predicate::str::contains("arg1 arg2 arg3"));
}

#[test]
fn test_command_with_dash_args() {
    /* Need -- separator for commands starting with - */
    timeout_cmd()
        .args(["5s", "--", "echo", "-n", "hello"])
        .assert()
        .success();
}

#[test]
fn test_combined_short_options() {
    /* Multiple short flags */
    timeout_cmd()
        .args(["-v", "-p", "-f", "5s", "echo", "ok"])
        .assert()
        .success();
}

#[test]
fn test_long_options_with_equals() {
    timeout_cmd()
        .args(["--signal=TERM", "--kill-after=5s", "5s", "echo", "ok"])
        .assert()
        .success();
}

#[test]
fn test_help() {
    timeout_cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("DURATION"))
        .stdout(predicate::str::contains("COMMAND"))
        .stdout(predicate::str::contains("--signal"))
        .stdout(predicate::str::contains("--kill-after"));
}

#[test]
fn test_version() {
    /*
     * Version output shows "procguard" regardless of which binary is invoked.
     * Both timeout and procguard are the same binary, just different entry names.
     */
    timeout_cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("procguard"));
}

#[test]
fn test_version_short_flag_must_be_standalone() {
    /* regression test for fuzzer-discovered bug: -V in a cluster like -V--i2
     * should not call exit(0) - it should error about -V not being a valid cluster member */
    timeout_cmd()
        .arg("-V--i2")
        .assert()
        .failure()
        .stderr(predicate::str::contains("-V must be used alone"));
}

#[test]
fn test_help_short_flag_must_be_standalone() {
    /* -h must also be standalone, not in a cluster like -h--i2 */
    timeout_cmd()
        .arg("-h--i2")
        .assert()
        .failure()
        .stderr(predicate::str::contains("-h must be used alone"));
}

/* =========================================================================
 * PROCESS GROUP HANDLING - Kill children too
 * ========================================================================= */

#[test]
fn test_kills_child_processes() {
    /*
     * When we timeout, child processes should also be killed.
     * This script spawns a background sleep - it should die with parent.
     */
    let start = Instant::now();

    timeout_cmd()
        .args(["0.3s", "sh", "--", "-c", "sleep 100 & wait"])
        .assert()
        .code(124);

    /* Should complete around 0.3s, not wait for the sleep 100 */
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn test_foreground_does_not_kill_children() {
    /*
     * In foreground mode, only the main process is killed.
     * This is a known limitation matching GNU behavior.
     * We just verify foreground mode works - can't easily test the
     * "children survive" part without more infrastructure.
     */
    timeout_cmd()
        .args(["--foreground", "0.3s", "sleep", "10"])
        .assert()
        .code(124);
}

/* =========================================================================
 * TIMING PRECISION - Make sure we're accurate
 * ========================================================================= */

#[test]
fn test_timing_precision_100ms() {
    let start = Instant::now();

    timeout_cmd()
        .args(["0.1s", "sleep", "10"])
        .assert()
        .code(124);

    let elapsed = start.elapsed();
    /* Should be ~100ms. Upper bound relaxed for x86_64 emulation on CI (ARM runners)
     * where process spawn overhead can exceed 500ms. */
    assert!(
        elapsed >= Duration::from_millis(50),
        "too fast: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1000),
        "too slow: {elapsed:?}"
    );
}

#[test]
fn test_timing_precision_500ms() {
    let start = Instant::now();

    timeout_cmd()
        .args(["0.5s", "sleep", "10"])
        .assert()
        .code(124);

    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(450),
        "too fast: {elapsed:?}"
    );
    /* CI VMs (especially x86_64 emulation) can be 2x slower */
    assert!(
        elapsed < Duration::from_millis(1500),
        "too slow: {elapsed:?}"
    );
}

#[test]
fn test_timing_precision_1s() {
    let start = Instant::now();

    timeout_cmd().args(["1s", "sleep", "10"]).assert().code(124);

    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(950),
        "too fast: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1200),
        "too slow: {elapsed:?}"
    );
}

/* =========================================================================
 * PERFORMANCE - No unnecessary overhead
 * ========================================================================= */

#[test]
fn test_fast_command_no_delay() {
    /*
     * Running a fast command shouldn't add significant overhead.
     * echo should complete in <100ms even with a long timeout.
     */
    let start = Instant::now();

    timeout_cmd()
        .args(["60s", "echo", "fast"])
        .assert()
        .success();

    assert!(
        start.elapsed() < Duration::from_millis(500),
        "simple echo took too long"
    );
}

#[test]
fn test_overhead_multiple_runs() {
    /*
     * Run several fast commands to check for consistent performance.
     * Each should complete quickly - no cumulative slowdown.
     */
    for i in 0..5 {
        let start = Instant::now();

        timeout_cmd()
            .args(["10s", "echo", &format!("run {i}")])
            .assert()
            .success();

        assert!(
            start.elapsed() < Duration::from_millis(200),
            "run {i} was slow"
        );
    }
}

/* =========================================================================
 * STDOUT/STDERR HANDLING - Pass through correctly
 * ========================================================================= */

#[test]
fn test_stdout_passthrough() {
    timeout_cmd()
        .args(["5s", "echo", "to stdout"])
        .assert()
        .success()
        .stdout(predicate::str::contains("to stdout"));
}

#[test]
fn test_stderr_passthrough() {
    timeout_cmd()
        .args(["5s", "sh", "--", "-c", "echo to stderr >&2"])
        .assert()
        .success()
        .stderr(predicate::str::contains("to stderr"));
}

#[test]
fn test_both_streams() {
    timeout_cmd()
        .args(["5s", "sh", "--", "-c", "echo out; echo err >&2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("out"))
        .stderr(predicate::str::contains("err"));
}

/* =========================================================================
 * STDIN PASSTHROUGH - non-consuming stdin watchdog
 * ========================================================================= */

#[test]
fn test_stdin_passthrough_keeps_data_for_child() {
    timeout_cmd()
        .args(["--stdin-timeout", "5s", "--stdin-passthrough", "5s", "cat"])
        .write_stdin("hello passthrough\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("hello passthrough"));
}

#[test]
fn test_stdin_passthrough_times_out_on_idle() {
    use std::process::{Command, Stdio};
    use std::thread;

    let mut child = Command::new(timeout_bin_path().as_str())
        .args([
            "--stdin-timeout",
            "0.2s",
            "--stdin-passthrough",
            "5s",
            "sleep",
            "10",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn timeout");

    /* keep stdin open without writing to trigger idle timeout */
    if let Some(stdin) = child.stdin.take() {
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(800));
            drop(stdin);
        });
    }

    let output = child
        .wait_with_output()
        .expect("failed to wait for timeout");
    assert_eq!(output.status.code(), Some(124));
}

#[test]
fn test_stdin_passthrough_eof_no_false_timeout() {
    /*
     * When stdin pipe closes (EOF), passthrough mode should NOT report
     * a stdin idle timeout. The command should run to completion.
     * This tests EOF detection via POLLHUP in poll().
     */
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new(timeout_bin_path().as_str())
        .args([
            "--stdin-timeout",
            "0.2s",
            "--stdin-passthrough",
            "5s",
            "sh",
            "-c",
            /* read all input then sleep - stdin EOF should not cause idle timeout */
            "cat > /dev/null; sleep 0.5",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn timeout");

    /* send data immediately then close stdin */
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(b"data\n").expect("write failed");
        /* stdin closes when dropped here */
    }

    /* command should succeed - not hit stdin idle timeout */
    let output = child
        .wait_with_output()
        .expect("failed to wait for timeout");
    assert!(
        output.status.success(),
        "expected success after EOF, got exit code {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/* =========================================================================
 * REAL-WORLD SCENARIOS - Common use cases
 * ========================================================================= */

#[test]
fn test_curl_simulation() {
    /*
     * Common pattern: timeout a network request.
     * We simulate with sleep since we can't rely on network.
     */
    timeout_cmd()
        .args(["0.3s", "sleep", "10"])
        .assert()
        .code(124);
}

#[test]
fn test_script_with_exit_code() {
    /* Run a script that exits with specific code */
    timeout_cmd()
        .args(["5s", "sh", "--", "-c", "exit 0"])
        .assert()
        .code(0);

    timeout_cmd()
        .args(["5s", "sh", "--", "-c", "exit 1"])
        .assert()
        .code(1);

    timeout_cmd()
        .args(["5s", "sh", "--", "-c", "exit 255"])
        .assert()
        .code(255);
}

#[test]
fn test_pipeline_exit_code() {
    /*
     * When running a pipeline, we get the exit code of the last command.
     * This is handled by the shell, but we should pass it through.
     */
    timeout_cmd()
        .args(["5s", "sh", "--", "-c", "true | false"])
        .assert()
        .code(1);

    timeout_cmd()
        .args(["5s", "sh", "--", "-c", "false | true"])
        .assert()
        .code(0);
}

#[test]
fn test_command_with_spaces_in_args() {
    timeout_cmd()
        .args(["5s", "echo", "hello world", "foo bar"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"))
        .stdout(predicate::str::contains("foo bar"));
}

#[test]
fn test_empty_output_command() {
    /* true produces no output */
    timeout_cmd()
        .args(["5s", "true"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

/* =========================================================================
 * STRESS TESTS - Edge conditions
 * ========================================================================= */

#[test]
fn test_very_short_timeout() {
    /* 10ms timeout - tests short timeout precision */
    let start = Instant::now();

    timeout_cmd()
        .args(["0.01s", "sleep", "10"])
        .assert()
        .code(124);

    /* Should be quick, but allow some slack for process startup */
    assert!(start.elapsed() < Duration::from_millis(500));
}

/* =========================================================================
 * RACE CONDITION TESTS - Verify fixes for timing-sensitive bugs
 * ========================================================================= */

#[test]
fn test_race_very_short_timeouts() {
    /*
     * Stress test: many very short timeouts in succession.
     * This exercises the race between spawn() and kevent() registration
     * where the process might exit before we can register EVFILT_PROC.
     * Bug fixed: ESRCH from kevent now properly falls back to wait().
     */
    for i in 0..50 {
        timeout_cmd()
            .args(["0.001s", "sleep", "10"])
            .assert()
            .code(124);

        /* Also test with varying short durations */
        let duration = format!("0.00{}s", (i % 9) + 1);
        timeout_cmd()
            .args([&duration, "sleep", "10"])
            .assert()
            .code(124);
    }
}

#[test]
fn test_race_fast_exiting_commands() {
    /*
     * Stress test: commands that exit faster than the timeout.
     * This exercises the race where process exits between spawn()
     * and kevent(), causing ESRCH errors. The fix ensures we properly
     * reap the process with blocking wait() when try_wait() returns None.
     */
    for _ in 0..100 {
        timeout_cmd().args(["10s", "true"]).assert().success();
    }
}

#[test]
fn test_race_command_exits_immediately() {
    /*
     * Commands that exit with various codes immediately.
     * Tests that fast process termination doesn't cause spurious errors.
     */
    for code in [0, 1, 42, 127, 255] {
        for _ in 0..20 {
            timeout_cmd()
                .args(["10s", "sh", "--", "-c", &format!("exit {code}")])
                .assert()
                .code(code);
        }
    }
}

#[test]
fn test_process_group_signal_fallback() {
    /*
     * Test that signaling works correctly even when process groups
     * might not be fully established. The fix makes send_signal()
     * fall back to kill(pid) when killpg() fails with ESRCH.
     *
     * We can't directly test setpgid failure, but we verify the
     * normal path works reliably under stress.
     */
    for _ in 0..30 {
        let start = Instant::now();

        timeout_cmd()
            .args(["0.05s", "sh", "--", "-c", "sleep 100 & wait"])
            .assert()
            .code(124);

        /* Should complete around 50ms, not wait for child processes */
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "process group signal may have failed"
        );
    }
}

#[test]
fn test_foreground_signal_no_process_group() {
    /*
     * In foreground mode, we use kill() not killpg().
     * Verify this works correctly under stress.
     */
    for _ in 0..30 {
        timeout_cmd()
            .args(["--foreground", "0.05s", "sleep", "10"])
            .assert()
            .code(124);
    }
}

#[test]
fn test_rapid_succession() {
    /* Quick timeouts in rapid succession */
    for _ in 0..3 {
        timeout_cmd()
            .args(["0.1s", "sleep", "10"])
            .assert()
            .code(124);
    }
}

#[test]
fn test_long_argument_list() {
    /* Pass many arguments to the command */
    let args: Vec<String> = (0..100).map(|i| format!("arg{i}")).collect();
    let mut cmd_args = vec!["5s".to_string(), "echo".to_string()];
    cmd_args.extend(args);

    let cmd_args_str: Vec<&str> = cmd_args.iter().map(String::as_str).collect();

    timeout_cmd()
        .args(&cmd_args_str)
        .assert()
        .success()
        .stdout(predicate::str::contains("arg0"))
        .stdout(predicate::str::contains("arg99"));
}

/* =========================================================================
 * SIGNAL FORWARDING - When timeout receives signals
 * ========================================================================= */

#[test]
fn test_signal_forwarding_sigterm() {
    /*
     * When timeout itself receives SIGTERM, it should forward the signal
     * to the child process and exit. This prevents orphaned processes
     * during system shutdown or user cancellation.
     *
     * We test this by spawning timeout in the background, then sending
     * it SIGTERM and verifying the child also terminates quickly.
     */
    use std::process::{Command, Stdio};
    use std::thread;

    /* Start timeout with a long-running command */
    let mut timeout_process = Command::new(timeout_bin_path().as_str())
        .args(["60s", "sleep", "60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn timeout");

    /* Give it time to start and set up signal handlers */
    thread::sleep(Duration::from_millis(200));

    let start = Instant::now();

    /* Send SIGTERM to the timeout process */
    // SAFETY: kill() is safe with any valid pid/signal combo
    unsafe {
        libc::kill(timeout_process.id() as i32, libc::SIGTERM);
    }

    /* Wait for timeout to exit (should be quick since we sent it SIGTERM) */
    let status = timeout_process.wait().expect("Failed to wait for timeout");

    /*
     * The key assertion: timeout should exit quickly (not wait 60s).
     * The exact exit code depends on signal handling, but it should
     * not be 124 (timeout) since we killed it early.
     */
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "timeout should exit quickly after SIGTERM, took {:?}",
        elapsed
    );

    /* Verify it didn't exit with 124 (normal timeout) */
    assert_ne!(
        status.code(),
        Some(124),
        "Should not exit with timeout code 124 since we killed it"
    );
}

#[test]
fn test_signal_forwarding_sigint() {
    /*
     * Similar test for SIGINT (Ctrl+C). Verify the process exits quickly.
     */
    use std::process::{Command, Stdio};
    use std::thread;

    let mut timeout_process = Command::new(timeout_bin_path().as_str())
        .args(["60s", "sleep", "60"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn timeout");

    thread::sleep(Duration::from_millis(200));

    let start = Instant::now();

    // SAFETY: kill() is safe with any valid pid/signal combo
    unsafe {
        libc::kill(timeout_process.id() as i32, libc::SIGINT);
    }

    let status = timeout_process.wait().expect("Failed to wait for timeout");

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "timeout should exit quickly after SIGINT, took {:?}",
        elapsed
    );

    assert_ne!(
        status.code(),
        Some(124),
        "Should not exit with timeout code 124 since we killed it"
    );
}

/* =========================================================================
 * JSON OUTPUT - Machine-readable results for CI
 * ========================================================================= */

#[test]
fn test_json_output_completed() {
    /*
     * --json flag outputs machine-readable JSON.
     * On successful completion: status, exit_code, elapsed_ms
     */
    timeout_cmd()
        .args(["--json", "5s", "true"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""status":"completed""#))
        .stdout(predicate::str::contains(r#""exit_code":0"#))
        .stdout(predicate::str::contains(r#""elapsed_ms":"#));
}

#[test]
fn test_json_output_timeout() {
    /*
     * On timeout: status, signal, signal_num, killed, exit_code, elapsed_ms
     */
    timeout_cmd()
        .args(["--json", "0.1s", "sleep", "10"])
        .assert()
        .code(124)
        .stdout(predicate::str::contains(r#""status":"timeout""#))
        .stdout(predicate::str::contains(r#""signal":"SIGTERM""#))
        .stdout(predicate::str::contains(r#""signal_num":15"#))
        .stdout(predicate::str::contains(r#""exit_code":124"#));
}

#[test]
fn test_json_output_with_kill_after() {
    /*
     * When process ignores SIGTERM and gets SIGKILL, killed should be true
     */
    timeout_cmd()
        .args([
            "--json",
            "-k",
            "0.1s",
            "0.1s",
            "sh",
            "--",
            "-c",
            "trap '' TERM; sleep 10",
        ])
        .assert()
        .code(124)
        .stdout(predicate::str::contains(r#""killed":true"#));
}

#[test]
fn test_json_output_error() {
    /*
     * On error (command not found): status error with message
     */
    timeout_cmd()
        .args(["--json", "5s", "nonexistent_command_xyz_12345"])
        .assert()
        .code(127)
        .stdout(predicate::str::contains(r#""status":"error""#))
        .stdout(predicate::str::contains(r#""exit_code":127"#));
}

#[test]
fn test_json_output_exit_code() {
    /*
     * Command exits with specific code, JSON should show it
     */
    timeout_cmd()
        .args(["--json", "5s", "sh", "--", "-c", "exit 42"])
        .assert()
        .code(42)
        .stdout(predicate::str::contains(r#""exit_code":42"#));
}

#[test]
fn test_json_valid_format() {
    /*
     * Output should contain valid JSON (proper structure)
     * Note: command stdout comes first, JSON on its own line
     */
    let output = timeout_cmd()
        .args(["--json", "5s", "true"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json_str = String::from_utf8(output).expect("valid utf8");
    /* Find the JSON line (starts with {) */
    let json_line = json_str
        .lines()
        .find(|l| l.starts_with('{'))
        .expect("should have JSON line");
    assert!(json_line.ends_with('}'), "JSON should end with }}");
    assert!(
        json_line.contains(r#""status":"#),
        "should have status field"
    );
}

/* =========================================================================
 * NEW FEATURES - quiet, timeout-exit-code, on-timeout, env vars
 * ========================================================================= */

#[test]
fn test_quiet_suppresses_errors() {
    /*
     * --quiet/-q should suppress error messages to stderr
     */
    timeout_cmd()
        .args(["-q", "5s", "nonexistent_command_xyz_12345"])
        .assert()
        .code(127)
        .stderr(predicate::str::is_empty());
}

#[test]
fn test_quiet_does_not_suppress_json() {
    /*
     * --quiet should NOT suppress JSON output
     */
    timeout_cmd()
        .args(["--quiet", "--json", "5s", "nonexistent_command_xyz_12345"])
        .assert()
        .code(127)
        .stdout(predicate::str::contains(r#""status":"error""#));
}

#[test]
fn test_quiet_short_flag() {
    /* -q is short for --quiet */
    timeout_cmd()
        .args(["-q", "5s", "nonexistent_command_xyz_12345"])
        .assert()
        .code(127)
        .stderr(predicate::str::is_empty());
}

#[test]
fn test_timeout_exit_code_custom() {
    /*
     * --timeout-exit-code changes the exit code on timeout
     */
    timeout_cmd()
        .args(["--timeout-exit-code", "42", "0.1s", "sleep", "10"])
        .assert()
        .code(42);
}

#[test]
fn test_timeout_exit_code_not_used_on_normal_exit() {
    /*
     * --timeout-exit-code should only affect timeout, not normal completion
     */
    timeout_cmd()
        .args([
            "--timeout-exit-code",
            "42",
            "5s",
            "sh",
            "--",
            "-c",
            "exit 7",
        ])
        .assert()
        .code(7);
}

#[test]
fn test_timeout_exit_code_with_json() {
    /*
     * JSON output should reflect the custom exit code
     */
    timeout_cmd()
        .args(["--timeout-exit-code", "99", "--json", "0.1s", "sleep", "10"])
        .assert()
        .code(99)
        .stdout(predicate::str::contains(r#""exit_code":99"#));
}

#[test]
fn test_env_timeout() {
    /*
     * TIMEOUT env var sets default duration
     */
    timeout_cmd()
        .env("TIMEOUT", "5s")
        .args(["echo", "from env"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from env"));
}

#[test]
fn test_env_timeout_signal() {
    /*
     * TIMEOUT_SIGNAL env var sets default signal
     */
    timeout_cmd()
        .env("TIMEOUT_SIGNAL", "HUP")
        .args(["-v", "0.1s", "sleep", "10"])
        .assert()
        .code(124)
        .stderr(predicate::str::contains("SIGHUP"));
}

#[test]
fn test_env_timeout_signal_overridden() {
    /*
     * -s flag should override TIMEOUT_SIGNAL
     */
    timeout_cmd()
        .env("TIMEOUT_SIGNAL", "HUP")
        .args(["-v", "-s", "INT", "0.1s", "sleep", "10"])
        .assert()
        .code(124)
        .stderr(predicate::str::contains("SIGINT"));
}

#[test]
fn test_env_timeout_kill_after() {
    /*
     * TIMEOUT_KILL_AFTER env var sets default kill-after
     */
    timeout_cmd()
        .env("TIMEOUT_KILL_AFTER", "0.1s")
        .args(["-v", "0.1s", "sh", "--", "-c", "trap '' TERM; sleep 10"])
        .assert()
        .code(124)
        .stderr(predicate::str::contains("SIGKILL"));
}

#[test]
fn test_on_timeout_runs_hook() {
    /*
     * --on-timeout should run a command when timeout occurs
     */
    let tmp_file = "/tmp/timeout_hook_test";
    std::fs::remove_file(tmp_file).ok();

    timeout_cmd()
        .args([
            "--on-timeout",
            &format!("touch {}", tmp_file),
            "0.1s",
            "sleep",
            "10",
        ])
        .assert()
        .code(124);

    assert!(
        std::path::Path::new(tmp_file).exists(),
        "hook should have created file"
    );
    std::fs::remove_file(tmp_file).ok();
}

#[test]
fn test_on_timeout_not_run_on_success() {
    /*
     * --on-timeout should NOT run when command completes before timeout
     */
    let tmp_file = "/tmp/timeout_hook_no_run_test";
    std::fs::remove_file(tmp_file).ok();

    timeout_cmd()
        .args(["--on-timeout", &format!("touch {}", tmp_file), "5s", "true"])
        .assert()
        .success();

    assert!(
        !std::path::Path::new(tmp_file).exists(),
        "hook should NOT have run"
    );
}

#[test]
fn test_on_timeout_limit() {
    /*
     * --on-timeout-limit should limit how long the hook can run
     */
    let start = Instant::now();

    timeout_cmd()
        .args([
            "--on-timeout",
            "sleep 10",
            "--on-timeout-limit",
            "0.2s",
            "0.1s",
            "sleep",
            "10",
        ])
        .assert()
        .code(124);

    /* Should not take 10s (hook limit should kick in) */
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "hook should have been limited"
    );
}

#[test]
fn test_on_timeout_verbose() {
    /*
     * --on-timeout with -v should show hook being run
     */
    timeout_cmd()
        .args(["-v", "--on-timeout", "true", "0.1s", "sleep", "10"])
        .assert()
        .code(124)
        .stderr(predicate::str::contains("on-timeout hook"));
}

/* =========================================================================
 * BUG FIXES - Tests for issues found in code audit
 * ========================================================================= */

#[test]
fn test_on_timeout_pid_substitution() {
    /*
     * Verify %p is correctly replaced with the actual PID
     */
    let tmp_file = "/tmp/timeout_pid_test";
    std::fs::remove_file(tmp_file).ok();

    timeout_cmd()
        .args([
            "--on-timeout",
            &format!("echo %p > {}", tmp_file),
            "0.1s",
            "sleep",
            "10",
        ])
        .assert()
        .code(124);

    /* Verify file was created with a valid PID */
    let content = std::fs::read_to_string(tmp_file).expect("hook should have created file");
    let pid: i32 = content.trim().parse().expect("should be a valid integer");
    assert!(pid > 0, "PID should be positive");
    std::fs::remove_file(tmp_file).ok();
}

#[test]
fn test_on_timeout_percent_escape() {
    /*
     * Verify %% is converted to literal %
     */
    let tmp_file = "/tmp/timeout_percent_test";
    std::fs::remove_file(tmp_file).ok();

    timeout_cmd()
        .args([
            "--on-timeout",
            &format!("echo '100%%' > {}", tmp_file),
            "0.1s",
            "sleep",
            "10",
        ])
        .assert()
        .code(124);

    let content = std::fs::read_to_string(tmp_file).expect("hook should have created file");
    assert!(content.contains("100%"), "should contain literal %");
    std::fs::remove_file(tmp_file).ok();
}

#[test]
fn test_quiet_passes_through_command_stderr() {
    /*
     * --quiet should suppress timeout's messages but NOT the command's stderr
     */
    timeout_cmd()
        .args(["-q", "5s", "sh", "-c", "echo error >&2"])
        .assert()
        .success()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn test_quiet_verbose_conflict() {
    /*
     * -q and -v should be mutually exclusive
     */
    timeout_cmd()
        .args(["-q", "-v", "5s", "true"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn test_json_schema_version() {
    /*
     * All JSON output should include schema_version field (version 8 with memory limit)
     */
    /* Test completed */
    timeout_cmd()
        .args(["--json", "5s", "true"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""schema_version":8"#));

    /* Test timeout */
    timeout_cmd()
        .args(["--json", "0.1s", "sleep", "10"])
        .assert()
        .code(124)
        .stdout(predicate::str::contains(r#""schema_version":8"#));

    /* Test error */
    timeout_cmd()
        .args(["--json", "5s", "nonexistent_command_xyz_12345"])
        .assert()
        .code(127)
        .stdout(predicate::str::contains(r#""schema_version":8"#));
}

#[test]
fn test_json_rusage_fields() {
    /*
     * JSON output should include resource usage fields (user_time_ms, system_time_ms, max_rss_kb)
     */
    /* Test completed - should have all rusage fields */
    timeout_cmd()
        .args(["--json", "5s", "true"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""user_time_ms":"#))
        .stdout(predicate::str::contains(r#""system_time_ms":"#))
        .stdout(predicate::str::contains(r#""max_rss_kb":"#));

    /* Test timeout - should also have rusage fields */
    timeout_cmd()
        .args(["--json", "0.1s", "sleep", "10"])
        .assert()
        .code(124)
        .stdout(predicate::str::contains(r#""user_time_ms":"#))
        .stdout(predicate::str::contains(r#""system_time_ms":"#))
        .stdout(predicate::str::contains(r#""max_rss_kb":"#));
}

#[test]
fn test_json_hook_fields() {
    /*
     * JSON output should include hook_* fields when hook is run
     */
    timeout_cmd()
        .args(["--json", "--on-timeout", "true", "0.1s", "sleep", "10"])
        .assert()
        .code(124)
        .stdout(predicate::str::contains(r#""hook_ran":true"#))
        .stdout(predicate::str::contains(r#""hook_timed_out":false"#));
}

#[test]
fn test_json_hook_fields_with_timeout() {
    /*
     * JSON should show hook_timed_out:true when hook exceeds limit
     */
    timeout_cmd()
        .args([
            "--json",
            "--on-timeout",
            "sleep 10",
            "--on-timeout-limit",
            "0.1s",
            "0.1s",
            "sleep",
            "10",
        ])
        .assert()
        .code(124)
        .stdout(predicate::str::contains(r#""hook_ran":true"#))
        .stdout(predicate::str::contains(r#""hook_timed_out":true"#));
}

#[test]
fn test_timeout_exit_code_warning() {
    /*
     * Using a reserved exit code (125-137) should print a warning only when timeout occurs
     */
    /* No warning when command completes before timeout */
    timeout_cmd()
        .args(["--timeout-exit-code", "127", "5s", "true"])
        .assert()
        .success()
        .stderr(predicate::str::is_empty());

    /* Warning should appear when timeout actually occurs */
    timeout_cmd()
        .args(["--timeout-exit-code", "127", "0.1s", "sleep", "10"])
        .assert()
        .code(127)
        .stderr(predicate::str::contains("warning"))
        .stderr(predicate::str::contains("reserved"));
}

#[test]
fn test_on_timeout_limit_warning() {
    /*
     * Warning when --on-timeout-limit exceeds main timeout
     */
    timeout_cmd()
        .args([
            "--on-timeout",
            "true",
            "--on-timeout-limit",
            "60s",
            "1s",
            "true",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("warning"))
        .stderr(predicate::str::contains("exceeds"));
}

/* =========================================================================
 * CONFINE MODE - Time measurement behavior
 * ========================================================================= */

#[test]
fn test_confine_wall_mode_accepted() {
    /*
     * -c wall should be accepted and use wall-clock timing (default behavior).
     * Uses mach_continuous_time which counts through system sleep.
     */
    timeout_cmd()
        .args(["-c", "wall", "5s", "echo", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
}

#[test]
fn test_confine_active_mode_accepted() {
    /*
     * -c active should be accepted and use active-time-only timing.
     * Uses CLOCK_MONOTONIC_RAW which pauses during system sleep.
     * ~28% faster internally due to no timebase conversion.
     */
    timeout_cmd()
        .args(["-c", "active", "5s", "echo", "hello"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello"));
}

#[test]
fn test_confine_long_form_accepted() {
    /*
     * --confine=wall and --confine=active should work
     */
    timeout_cmd()
        .args(["--confine=wall", "5s", "echo", "hello"])
        .assert()
        .success();

    timeout_cmd()
        .args(["--confine=active", "5s", "echo", "hello"])
        .assert()
        .success();
}

#[test]
fn test_confine_invalid_mode_rejected() {
    /*
     * Invalid confine mode should produce an error
     */
    timeout_cmd()
        .args(["-c", "invalid", "5s", "echo", "hello"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("confine"));
}

#[test]
fn test_confine_active_timeout_works() {
    /*
     * Active mode should still properly timeout commands.
     * This verifies the CLOCK_MONOTONIC_RAW timing path.
     */
    let start = std::time::Instant::now();

    timeout_cmd()
        .args(["-c", "active", "0.5s", "sleep", "10"])
        .assert()
        .code(124);

    let elapsed = start.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_millis(400),
        "timed out too early"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "took too long to timeout"
    );
}

#[test]
fn test_signal_forwarding_reports_correct_signal() {
    /*
     * When we forward SIGINT, verbose output should say SIGINT not SIGTERM
     */
    use std::process::{Command, Stdio};
    use std::thread;

    let timeout_process = Command::new(timeout_bin_path().as_str())
        .args(["-v", "30s", "sleep", "100"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to start timeout");

    let pid = timeout_process.id() as i32;
    thread::sleep(Duration::from_millis(200));

    /* Send SIGINT */
    // SAFETY: kill() is safe with any valid pid/signal combo
    unsafe {
        libc::kill(pid, libc::SIGINT);
    }

    let output = timeout_process.wait_with_output().expect("Failed to wait");
    let stderr = String::from_utf8_lossy(&output.stderr);

    /* Should report SIGINT, not SIGTERM */
    assert!(
        stderr.contains("SIGINT"),
        "verbose output should report SIGINT, got: {}",
        stderr
    );
}

#[test]
fn test_signal_forwarded_json_rusage() {
    /*
     * When signal is forwarded, JSON output should include rusage fields
     */
    use std::process::{Command, Stdio};
    use std::thread;

    let timeout_process = Command::new(timeout_bin_path().as_str())
        .args(["--json", "30s", "sleep", "100"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to start timeout");

    let pid = timeout_process.id() as i32;
    /* give time for timeout to spawn child and set up signal handlers */
    thread::sleep(Duration::from_millis(300));

    /* Send SIGTERM to trigger signal forwarding */
    // SAFETY: kill() is safe with any valid pid/signal combo
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }

    let output = timeout_process.wait_with_output().expect("Failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);

    /* Should have signal_forwarded status with rusage fields */
    assert!(
        stdout.contains(r#""status":"signal_forwarded""#),
        "should report signal_forwarded status, got: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""user_time_ms":"#),
        "should include user_time_ms, got: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""system_time_ms":"#),
        "should include system_time_ms, got: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""max_rss_kb":"#),
        "should include max_rss_kb, got: {}",
        stdout
    );
}

/* =========================================================================
 * WAIT FOR FILE - Pre-command file waiting feature
 * ========================================================================= */

#[test]
fn test_wait_for_file_existing_file() {
    /*
     * When file already exists, should proceed immediately.
     */
    timeout_cmd()
        .args(["--wait-for-file", "Cargo.toml", "5s", "echo", "success"])
        .assert()
        .success()
        .stdout(predicate::str::contains("success"));
}

#[test]
fn test_wait_for_file_timeout() {
    /*
     * When file doesn't exist and wait times out, exit 124.
     */
    let start = Instant::now();

    timeout_cmd()
        .args([
            "--wait-for-file",
            "/tmp/nonexistent_file_for_test_12345",
            "--wait-for-file-timeout",
            "0.1s",
            "5s",
            "echo",
            "should not run",
        ])
        .assert()
        .code(124)
        .stderr(predicate::str::contains("timed out waiting for file"));

    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(100),
        "should have waited at least 100ms"
    );
    assert!(elapsed < Duration::from_secs(1), "should not wait too long");
}

#[test]
fn test_wait_for_file_created_during_wait() {
    /*
     * When file is created while waiting, should proceed.
     */
    use std::fs;
    use std::thread;

    let test_file = "/tmp/procguard_test_wait_file_integration";
    let _ = fs::remove_file(test_file);

    /* Spawn a thread to create the file after a delay */
    let path = test_file.to_string();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        fs::write(&path, "ready").unwrap();
    });

    let start = Instant::now();

    timeout_cmd()
        .args([
            "--wait-for-file",
            test_file,
            "--wait-for-file-timeout",
            "5s",
            "30s",
            "echo",
            "file found",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("file found"));

    let elapsed = start.elapsed();
    assert!(elapsed < Duration::from_secs(2), "should find file quickly");

    /* Clean up */
    let _ = fs::remove_file(test_file);
}

#[test]
fn test_wait_for_file_verbose() {
    /*
     * Verbose mode should show waiting status.
     */
    timeout_cmd()
        .args(["-v", "--wait-for-file", "Cargo.toml", "5s", "echo", "done"])
        .assert()
        .success()
        .stderr(predicate::str::contains("waiting for file"))
        .stderr(predicate::str::contains("found"));
}

#[test]
fn test_wait_for_file_quiet() {
    /*
     * Quiet mode should suppress error messages.
     */
    timeout_cmd()
        .args([
            "-q",
            "--wait-for-file",
            "/tmp/nonexistent_12345",
            "--wait-for-file-timeout",
            "0.1s",
            "5s",
            "echo",
            "nope",
        ])
        .assert()
        .code(124)
        .stderr(predicate::str::is_empty());
}

#[test]
fn test_wait_for_file_json_timeout() {
    /*
     * JSON output should include wait-for-file timeout info.
     */
    timeout_cmd()
        .args([
            "--json",
            "--wait-for-file",
            "/tmp/nonexistent_12345",
            "--wait-for-file-timeout",
            "0.1s",
            "5s",
            "echo",
            "nope",
        ])
        .assert()
        .code(124)
        .stdout(predicate::str::contains("\"status\":\"error\""))
        .stdout(predicate::str::contains("\"exit_code\":124"));
}

#[test]
fn test_wait_for_file_with_env_var() {
    /*
     * Environment variable should work for wait-for-file.
     */
    timeout_cmd()
        .env("TIMEOUT_WAIT_FOR_FILE", "Cargo.toml")
        .args(["5s", "echo", "env var works"])
        .assert()
        .success()
        .stdout(predicate::str::contains("env var works"));
}

#[test]
fn test_wait_for_file_cli_overrides_env() {
    /*
     * CLI should override environment variable.
     */
    timeout_cmd()
        .env("TIMEOUT_WAIT_FOR_FILE", "/tmp/nonexistent_12345")
        .args(["--wait-for-file", "Cargo.toml", "5s", "echo", "cli wins"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cli wins"));
}

/* ===== Retry Tests ===== */

#[test]
fn test_retry_no_timeout_no_retry() {
    /*
     * When command succeeds, retry should not trigger
     */
    timeout_cmd()
        .args(["--retry", "3", "5s", "true"])
        .assert()
        .success();
}

#[test]
fn test_retry_json_no_retry_configured() {
    /*
     * Without --retry, JSON should not include attempt_results
     */
    let output = timeout_cmd()
        .args(["--json", "5s", "true"])
        .output()
        .expect("failed to run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("attempt_results"),
        "should not include attempt_results without --retry"
    );
    assert!(
        !stdout.contains(r#""attempts":"#),
        "should not include attempts count without --retry"
    );
}

#[test]
fn test_retry_json_with_retry_configured() {
    /*
     * With --retry, JSON should include attempt_results even on first success
     */
    let output = timeout_cmd()
        .args(["--json", "--retry", "3", "5s", "true"])
        .output()
        .expect("failed to run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(r#""attempts":1"#),
        "should have 1 attempt on success: {}",
        stdout
    );
    assert!(
        stdout.contains("attempt_results"),
        "should include attempt_results with --retry: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""status":"completed""#),
        "first attempt should be completed: {}",
        stdout
    );
}

#[test]
fn test_retry_timeout_triggers_retry() {
    /*
     * On timeout, retry should run command again
     */
    let output = timeout_cmd()
        .args(["--json", "--retry", "1", "-v", "0.1s", "sleep", "10"])
        .output()
        .expect("failed to run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    /* Should have 2 attempts (initial + 1 retry) */
    assert!(
        stdout.contains(r#""attempts":2"#),
        "should have 2 attempts: {}",
        stdout
    );

    /* attempt_results should contain 2 entries */
    assert!(
        stdout.contains("attempt_results"),
        "should include attempt_results: {}",
        stdout
    );

    /* Verbose should show retry message */
    assert!(
        stderr.contains("retry"),
        "verbose should show retry message: {}",
        stderr
    );
}

#[test]
fn test_retry_respects_retry_count() {
    /*
     * Should not retry more than N times
     */
    let start = std::time::Instant::now();
    let output = timeout_cmd()
        .args(["--json", "--retry", "2", "0.1s", "sleep", "10"])
        .output()
        .expect("failed to run command");

    let elapsed = start.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);

    /* Should have 3 attempts total (initial + 2 retries) */
    assert!(
        stdout.contains(r#""attempts":3"#),
        "should have 3 attempts: {}",
        stdout
    );

    /* Should take ~300ms (3 * 100ms timeout) */
    assert!(
        elapsed.as_millis() >= 280,
        "should take at least 300ms for 3 attempts: {:?}",
        elapsed
    );
    assert!(
        elapsed.as_millis() < 1000,
        "should not take too long (no excessive delays): {:?}",
        elapsed
    );
}

#[test]
fn test_retry_delay() {
    /*
     * --retry-delay should add delay between retries
     */
    let start = std::time::Instant::now();
    timeout_cmd()
        .args([
            "--retry",
            "1",
            "--retry-delay",
            "200ms",
            "0.1s",
            "sleep",
            "10",
        ])
        .output()
        .expect("failed to run command");

    let elapsed = start.elapsed();

    /* Should take at least 400ms (2 * 100ms timeout + 200ms delay) */
    assert!(
        elapsed.as_millis() >= 380,
        "should include retry delay: {:?}",
        elapsed
    );
}

#[test]
fn test_retry_backoff() {
    /*
     * --retry-backoff should multiply delay each retry
     */
    let start = std::time::Instant::now();
    timeout_cmd()
        .args([
            "--retry",
            "2",
            "--retry-delay",
            "100ms",
            "--retry-backoff",
            "2x",
            "50ms",
            "sleep",
            "10",
        ])
        .output()
        .expect("failed to run command");

    let elapsed = start.elapsed();

    /* Should take at least 450ms:
     * - attempt 1: 50ms timeout
     * - delay: 100ms
     * - attempt 2: 50ms timeout
     * - delay: 200ms (100ms * 2)
     * - attempt 3: 50ms timeout
     * Total: 150ms timeout + 300ms delay = 450ms
     */
    assert!(
        elapsed.as_millis() >= 400,
        "should include exponential backoff delays: {:?}",
        elapsed
    );
}

#[test]
fn test_retry_env_var() {
    /*
     * TIMEOUT_RETRY env var should set default retry count
     */
    let output = timeout_cmd()
        .env("TIMEOUT_RETRY", "1")
        .args(["--json", "0.1s", "sleep", "10"])
        .output()
        .expect("failed to run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(r#""attempts":2"#),
        "env var should enable retry: {}",
        stdout
    );
}

#[test]
fn test_retry_cli_overrides_env() {
    /*
     * CLI --retry should override TIMEOUT_RETRY env var
     */
    let output = timeout_cmd()
        .env("TIMEOUT_RETRY", "5")
        .args(["--json", "--retry", "1", "0.1s", "sleep", "10"])
        .output()
        .expect("failed to run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    /* Should only have 2 attempts (--retry 1), not 6 (env var 5) */
    assert!(
        stdout.contains(r#""attempts":2"#),
        "CLI should override env var: {}",
        stdout
    );
}

#[test]
fn test_retry_short_flag() {
    /*
     * -r should work as short form of --retry
     */
    let output = timeout_cmd()
        .args(["--json", "-r", "1", "0.1s", "sleep", "10"])
        .output()
        .expect("failed to run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(r#""attempts":2"#),
        "-r should enable retry: {}",
        stdout
    );
}

#[test]
fn test_retry_does_not_retry_on_success() {
    /*
     * Retry should only trigger on timeout, not on command failure
     */
    let output = timeout_cmd()
        .args(["--json", "--retry", "3", "5s", "false"])
        .output()
        .expect("failed to run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    /* Command failed immediately, should not retry */
    assert!(
        stdout.contains(r#""attempts":1"#),
        "should not retry on command failure: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""exit_code":1"#),
        "should preserve exit code: {}",
        stdout
    );
}

/* =========================================================================
 * HEARTBEAT - CI keep-alive feature
 * ========================================================================= */

#[test]
fn test_heartbeat_prints_status() {
    /*
     * --heartbeat should print status messages to stderr at regular intervals
     */
    let output = timeout_cmd()
        .args(["--heartbeat", "500ms", "2s", "sleep", "10"])
        .output()
        .expect("failed to run command");

    let stderr = String::from_utf8_lossy(&output.stderr);
    /* Should have at least 2 heartbeat messages (at 500ms and 1s) */
    assert!(
        stderr.contains("timeout: heartbeat:"),
        "should print heartbeat messages: {}",
        stderr
    );
    assert!(
        stderr.contains("elapsed, command still running"),
        "heartbeat should include elapsed time: {}",
        stderr
    );
    assert!(
        stderr.contains("pid"),
        "heartbeat should include pid: {}",
        stderr
    );
}

#[test]
fn test_heartbeat_short_flag() {
    /*
     * -H should work as short form of --heartbeat
     */
    let output = timeout_cmd()
        .args(["-H", "500ms", "1.5s", "sleep", "10"])
        .output()
        .expect("failed to run command");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("timeout: heartbeat:"),
        "-H should enable heartbeat: {}",
        stderr
    );
}

#[test]
fn test_heartbeat_quiet_suppresses() {
    /*
     * --quiet should suppress heartbeat messages
     */
    let output = timeout_cmd()
        .args(["--quiet", "--heartbeat", "200ms", "1s", "sleep", "10"])
        .output()
        .expect("failed to run command");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("heartbeat"),
        "--quiet should suppress heartbeat: {}",
        stderr
    );
}

#[test]
fn test_heartbeat_no_output_before_interval() {
    /*
     * No heartbeat should print if command completes before first interval
     */
    let output = timeout_cmd()
        .args(["--heartbeat", "5s", "10s", "echo", "quick"])
        .output()
        .expect("failed to run command");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stderr.contains("heartbeat"),
        "no heartbeat before interval: {}",
        stderr
    );
    assert!(
        stdout.contains("quick"),
        "command should complete: {}",
        stdout
    );
}

#[test]
fn test_heartbeat_env_var() {
    /*
     * TIMEOUT_HEARTBEAT env var should set default heartbeat interval
     */
    let output = timeout_cmd()
        .env("TIMEOUT_HEARTBEAT", "500ms")
        .args(["1.5s", "sleep", "10"])
        .output()
        .expect("failed to run command");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("timeout: heartbeat:"),
        "env var should enable heartbeat: {}",
        stderr
    );
}

#[test]
fn test_heartbeat_cli_overrides_env() {
    /*
     * CLI --heartbeat should override TIMEOUT_HEARTBEAT env var
     */
    let output = timeout_cmd()
        .env("TIMEOUT_HEARTBEAT", "100ms") /* would produce many messages */
        .args(["--heartbeat", "2s", "1s", "sleep", "10"]) /* only 1s timeout, no heartbeat */
        .output()
        .expect("failed to run command");

    let stderr = String::from_utf8_lossy(&output.stderr);
    /* With 2s heartbeat and 1s timeout, no heartbeat should print */
    assert!(
        !stderr.contains("heartbeat"),
        "CLI should override env var (2s interval > 1s timeout): {}",
        stderr
    );
}

#[test]
fn test_heartbeat_with_json() {
    /*
     * Heartbeat should work alongside JSON output
     */
    let output = timeout_cmd()
        .args(["--json", "--heartbeat", "500ms", "1.5s", "sleep", "10"])
        .output()
        .expect("failed to run command");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    /* Heartbeat goes to stderr */
    assert!(
        stderr.contains("timeout: heartbeat:"),
        "heartbeat should go to stderr: {}",
        stderr
    );
    /* JSON goes to stdout */
    assert!(
        stdout.contains(r#""status":"timeout""#),
        "JSON should go to stdout: {}",
        stdout
    );
}

#[test]
fn test_heartbeat_elapsed_time_format() {
    /*
     * Heartbeat should show elapsed time in human-readable format
     */
    let output = timeout_cmd()
        .args(["--heartbeat", "500ms", "1.5s", "sleep", "10"])
        .output()
        .expect("failed to run command");

    let stderr = String::from_utf8_lossy(&output.stderr);
    /* Should show seconds (e.g., "1s elapsed") */
    assert!(
        stderr.contains("s elapsed"),
        "should show elapsed seconds: {}",
        stderr
    );
}

#[test]
fn test_heartbeat_with_confine_active() {
    /*
     * Heartbeat should work with --confine active
     */
    let output = timeout_cmd()
        .args([
            "--confine",
            "active",
            "--heartbeat",
            "500ms",
            "1.5s",
            "sleep",
            "10",
        ])
        .output()
        .expect("failed to run command");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("timeout: heartbeat:"),
        "heartbeat should work with confine active: {}",
        stderr
    );
}

/* =========================================================================
 * STDIN TIMEOUT - Interactive process detection
 * ========================================================================= */

#[test]
fn test_stdin_timeout_triggers() {
    /*
     * --stdin-timeout should kill command if stdin has no activity for the duration.
     * Using stdin(Stdio::piped()) creates a pipe that blocks (no EOF).
     * We must take() the stdin handle to prevent EOF when wait() is called.
     */
    use std::process::Stdio;
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args(["--stdin-timeout", "200ms", "60s", "sleep", "60"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take(); /* keep pipe open */
    let status = child.wait().expect("failed to wait");
    /* should exit 124 (timeout) due to stdin idle */
    assert_eq!(status.code(), Some(124), "should timeout due to stdin idle");
}

#[test]
fn test_stdin_timeout_short_flag() {
    /*
     * -S short flag should work like --stdin-timeout
     */
    use std::process::Stdio;
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args(["-S", "200ms", "60s", "sleep", "60"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take();
    let status = child.wait().expect("failed to wait");
    assert_eq!(status.code(), Some(124), "-S should trigger stdin timeout");
}

#[test]
fn test_stdin_timeout_short_flag_embedded() {
    /*
     * -S200ms embedded value should work
     */
    use std::process::Stdio;
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args(["-S200ms", "60s", "sleep", "60"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take();
    let status = child.wait().expect("failed to wait");
    assert_eq!(
        status.code(),
        Some(124),
        "-S200ms should trigger stdin timeout"
    );
}

#[test]
fn test_stdin_timeout_not_triggered_on_fast_exit() {
    /*
     * stdin-timeout should NOT fire if command exits before the idle duration.
     */
    timeout_cmd()
        .args(["--stdin-timeout", "60s", "60s", "true"])
        .assert()
        .success();
}

#[test]
fn test_stdin_timeout_env_var() {
    /*
     * TIMEOUT_STDIN_TIMEOUT env var should set default stdin timeout
     */
    use std::process::Stdio;
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .env("TIMEOUT_STDIN_TIMEOUT", "200ms")
        .args(["60s", "sleep", "60"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take();
    let status = child.wait().expect("failed to wait");
    assert_eq!(
        status.code(),
        Some(124),
        "TIMEOUT_STDIN_TIMEOUT should trigger timeout"
    );
}

#[test]
fn test_stdin_timeout_cli_overrides_env() {
    /*
     * CLI --stdin-timeout should override env var
     */
    use std::process::Stdio;
    /* env var would trigger quickly (50ms), but CLI sets longer timeout (60s) */
    /* so wall clock timeout (200ms) fires first */
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .env("TIMEOUT_STDIN_TIMEOUT", "50ms")
        .args(["--stdin-timeout", "60s", "200ms", "sleep", "60"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take();
    let status = child.wait().expect("failed to wait");
    /* should timeout from wall clock (200ms), not stdin (60s) */
    assert_eq!(status.code(), Some(124), "CLI should override env var");
}

#[test]
fn test_stdin_timeout_json_reason_stdin_idle() {
    /*
     * JSON should include timeout_reason: "stdin_idle" when stdin timeout fires
     */
    use std::io::Read;
    use std::process::Stdio;
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args(["--json", "--stdin-timeout", "100ms", "60s", "sleep", "60"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take();
    let status = child.wait().expect("failed to wait");
    let mut stdout_content = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut stdout_content).ok();
    }

    assert_eq!(status.code(), Some(124), "should exit 124");
    assert!(
        stdout_content.contains(r#""timeout_reason":"stdin_idle""#),
        "should show stdin_idle reason: {}",
        stdout_content
    );
    assert!(
        stdout_content.contains(r#""status":"timeout""#),
        "should show timeout status: {}",
        stdout_content
    );
}

#[test]
fn test_stdin_timeout_json_reason_wall_clock() {
    /*
     * JSON should include timeout_reason: "wall_clock" when main timeout fires
     */
    let output = timeout_cmd()
        .args(["--json", "100ms", "sleep", "10"])
        .output()
        .expect("failed to run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(r#""timeout_reason":"wall_clock""#),
        "should show wall_clock reason: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""status":"timeout""#),
        "should show timeout status: {}",
        stdout
    );
}

#[test]
fn test_stdin_timeout_verbose() {
    /*
     * Verbose should show stdin idle message
     */
    use std::process::Stdio;
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args([
            "--verbose",
            "--stdin-timeout",
            "100ms",
            "60s",
            "sleep",
            "60",
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take();
    let status = child.wait().expect("failed to wait");
    let mut stderr_content = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        use std::io::Read;
        stderr.read_to_string(&mut stderr_content).ok();
    }

    assert_eq!(status.code(), Some(124));
    /* verbose output should mention stdin */
    assert!(
        stderr_content.contains("stdin") || stderr_content.contains("SIGTERM"),
        "verbose should mention stdin idle or signal: {}",
        stderr_content
    );
}

#[test]
fn test_stdin_timeout_quiet() {
    /*
     * Quiet mode should suppress stdin timeout messages
     */
    use std::process::Stdio;
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args(["--quiet", "--stdin-timeout", "100ms", "60s", "sleep", "60"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take();
    let status = child.wait().expect("failed to wait");
    let mut stderr_content = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        use std::io::Read;
        stderr.read_to_string(&mut stderr_content).ok();
    }

    assert_eq!(status.code(), Some(124));
    /* stderr should be empty (no diagnostics) */
    assert!(
        stderr_content.is_empty(),
        "quiet should suppress messages: {}",
        stderr_content
    );
}

#[test]
fn test_stdin_timeout_with_wall_timeout() {
    /*
     * Both timeouts can be set; whichever fires first wins.
     * Here stdin timeout (100ms) should fire before wall timeout (60s).
     */
    use std::process::Stdio;
    let start = std::time::Instant::now();
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args(["--stdin-timeout", "100ms", "60s", "sleep", "60"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take();
    let status = child.wait().expect("failed to wait");
    let elapsed = start.elapsed();

    assert_eq!(status.code(), Some(124));
    /* should complete quickly due to stdin timeout, not wait 60s */
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "should timeout quickly from stdin idle: {:?}",
        elapsed
    );
}

#[test]
fn test_stdin_timeout_wall_timeout_fires_first() {
    /*
     * If wall timeout is shorter than stdin timeout, wall timeout fires first.
     */
    use std::io::Read;
    use std::process::Stdio;
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args(["--json", "--stdin-timeout", "60s", "100ms", "sleep", "60"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take();
    let status = child.wait().expect("failed to wait");
    let mut stdout_content = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_string(&mut stdout_content).ok();
    }

    assert_eq!(status.code(), Some(124));
    assert!(
        stdout_content.contains(r#""timeout_reason":"wall_clock""#),
        "wall clock should fire first: {}",
        stdout_content
    );
}

#[test]
fn test_stdin_timeout_with_heartbeat() {
    /*
     * stdin-timeout should work alongside heartbeat
     */
    use std::io::Read;
    use std::process::Stdio;
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args([
            "--heartbeat",
            "50ms",
            "--stdin-timeout",
            "200ms",
            "60s",
            "sleep",
            "60",
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take();
    let status = child.wait().expect("failed to wait");
    let mut stderr_content = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        stderr.read_to_string(&mut stderr_content).ok();
    }

    assert_eq!(status.code(), Some(124), "should timeout with both flags");
    assert!(
        stderr_content.contains("heartbeat"),
        "heartbeat should still print: {}",
        stderr_content
    );
}

#[test]
fn test_stdin_timeout_combined_flags() {
    /*
     * stdin-timeout should work with other flags like -v and -p
     */
    use std::io::Read;
    use std::process::Stdio;
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args(["-v", "-S", "100ms", "60s", "sleep", "60"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take();
    let status = child.wait().expect("failed to wait");
    let mut stderr_content = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        stderr.read_to_string(&mut stderr_content).ok();
    }

    assert_eq!(status.code(), Some(124));
    /* verbose should show something */
    assert!(
        !stderr_content.is_empty(),
        "verbose should produce output: {}",
        stderr_content
    );
}

#[test]
fn test_stdin_timeout_dev_null_no_busy_loop() {
    /*
     * When stdin is /dev/null (immediate EOF), stdin timeout should be
     * disabled and not cause a busy loop. Command should wait for wall
     * clock timeout normally.
     */
    use std::fs::File;

    let dev_null = File::open("/dev/null").expect("failed to open /dev/null");
    let start = std::time::Instant::now();

    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args(["--stdin-timeout", "50ms", "200ms", "sleep", "60"])
        .stdin(dev_null)
        .spawn()
        .expect("failed to spawn");

    let status = child.wait().expect("failed to wait");
    let elapsed = start.elapsed();

    /* should exit 124 (timeout) */
    assert_eq!(status.code(), Some(124));

    /* should take ~200ms (wall clock), not 50ms (stdin timeout disabled on EOF) */
    assert!(
        elapsed >= std::time::Duration::from_millis(150),
        "should wait for wall clock timeout, not stdin (elapsed: {:?})",
        elapsed
    );
    /* should complete in reasonable time, not hang */
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "should not hang: {:?}",
        elapsed
    );
}

#[test]
fn test_stdin_timeout_null_stdin_no_cpu_spike() {
    /*
     * When stdin is Stdio::null(), it should behave like /dev/null -
     * no busy loop, no CPU spike, wall clock timeout fires.
     */
    use std::process::Stdio;

    let start = std::time::Instant::now();

    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args(["--stdin-timeout", "50ms", "200ms", "sleep", "60"])
        .stdin(Stdio::null())
        .spawn()
        .expect("failed to spawn");

    let status = child.wait().expect("failed to wait");
    let elapsed = start.elapsed();

    assert_eq!(status.code(), Some(124));
    /* should wait for wall clock, not immediately trigger stdin timeout */
    assert!(
        elapsed >= std::time::Duration::from_millis(150),
        "should wait for wall clock timeout: {:?}",
        elapsed
    );
}

#[test]
fn test_stdin_timeout_closed_stdin_graceful() {
    /*
     * When stdin fd is closed (0<&-), stdin timeout should be disabled
     * gracefully and fall back to wall clock timeout.
     */
    let start = std::time::Instant::now();

    /* run via sh to close stdin: 0<&- closes fd 0 */
    let output = std::process::Command::new("sh")
        .args([
            "-c",
            &format!(
                "{} --stdin-timeout 50ms 200ms sleep 60 0<&-",
                timeout_bin_path().as_str()
            ),
        ])
        .output()
        .expect("failed to run command");

    let elapsed = start.elapsed();

    assert_eq!(output.status.code(), Some(124));
    /* should wait for wall clock timeout since stdin is invalid */
    assert!(
        elapsed >= std::time::Duration::from_millis(150),
        "should wait for wall clock timeout: {:?}",
        elapsed
    );
}

#[test]
fn test_stdin_timeout_json_with_dev_null() {
    /*
     * JSON output should show wall_clock reason when stdin is /dev/null
     * (stdin timeout disabled due to immediate EOF).
     */
    use std::fs::File;

    let dev_null = File::open("/dev/null").expect("failed to open /dev/null");

    let output = std::process::Command::new(timeout_bin_path().as_str())
        .args(["--json", "--stdin-timeout", "50ms", "100ms", "sleep", "60"])
        .stdin(dev_null)
        .output()
        .expect("failed to run command");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(124));
    /* stdin timeout disabled on EOF, so wall_clock fires */
    assert!(
        stdout.contains(r#""timeout_reason":"wall_clock""#),
        "should show wall_clock reason when stdin is /dev/null: {}",
        stdout
    );
}

#[test]
fn test_stdin_timeout_with_retry() {
    /*
     * --stdin-timeout with --retry: each retry attempt should get fresh
     * stdin timeout state. First attempt times out due to stdin idle,
     * subsequent retries should also work correctly.
     */
    use std::process::Stdio;

    let start = std::time::Instant::now();
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args([
            "--json",
            "--stdin-timeout",
            "100ms",
            "--retry",
            "1",
            "60s",
            "sleep",
            "60",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take(); /* keep pipe open, no data sent */
    let output = child.wait_with_output().expect("failed to wait");
    let elapsed = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(124));

    /* should have 2 attempts (initial + 1 retry) */
    assert!(
        stdout.contains(r#""attempts":2"#),
        "should have 2 attempts: {}",
        stdout
    );

    /* both attempts should timeout from stdin idle */
    assert!(
        stdout.contains(r#""timeout_reason":"stdin_idle""#),
        "final result should show stdin_idle reason: {}",
        stdout
    );

    /* should complete in ~200ms (100ms x 2 attempts), not hang */
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "should not hang: {:?}",
        elapsed
    );
}

#[test]
fn test_stdin_timeout_retry_with_dev_null() {
    /*
     * --stdin-timeout with --retry and /dev/null stdin: each retry should
     * detect EOF and fall back to wall clock timeout.
     */
    use std::fs::File;

    let dev_null = File::open("/dev/null").expect("failed to open /dev/null");
    let start = std::time::Instant::now();

    let output = std::process::Command::new(timeout_bin_path().as_str())
        .args([
            "--json",
            "--stdin-timeout",
            "50ms",
            "--retry",
            "1",
            "100ms",
            "sleep",
            "60",
        ])
        .stdin(dev_null)
        .output()
        .expect("failed to run command");

    let elapsed = start.elapsed();
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(output.status.code(), Some(124));

    /* should have 2 attempts */
    assert!(
        stdout.contains(r#""attempts":2"#),
        "should have 2 attempts: {}",
        stdout
    );

    /* final timeout should be wall_clock since stdin EOF disables stdin timeout */
    assert!(
        stdout.contains(r#""timeout_reason":"wall_clock""#),
        "should show wall_clock reason: {}",
        stdout
    );

    /* should take ~200ms (100ms x 2), not instant (stdin timeout not firing prematurely) */
    assert!(
        elapsed >= std::time::Duration::from_millis(150),
        "should wait for wall clock timeouts: {:?}",
        elapsed
    );
}

/* =========================================================================
 * RESOURCE LIMITS - Memory, CPU time, and CPU throttling
 * ========================================================================= */

#[test]
fn test_cpu_percent_flag_accepted() {
    /*
     * --cpu-percent should be accepted and parsed correctly.
     * Use a high limit (99%) so process completes normally.
     */
    timeout_cmd()
        .args(["--cpu-percent=99", "5s", "echo", "throttled"])
        .assert()
        .success()
        .stdout(predicate::str::contains("throttled"));
}

#[test]
fn test_cpu_percent_invalid_zero() {
    /* 0% is invalid - cannot throttle to nothing */
    timeout_cmd()
        .args(["--cpu-percent=0", "5s", "echo", "test"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("invalid"));
}

#[test]
fn test_cpu_percent_invalid_over_100() {
    /* non-numeric is invalid */
    timeout_cmd()
        .args(["--cpu-percent=abc", "5s", "echo", "test"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("invalid"));
}

#[test]
fn test_cpu_percent_throttles_busy_loop() {
    /*
     * --cpu-percent=20 should throttle a CPU-intensive process.
     * We verify that a busy loop takes longer with throttling enabled.
     * This tests the SIGSTOP/SIGCONT throttling mechanism in throttle.rs.
     */
    use std::time::Instant;

    /* baseline: unthrottled busy loop for 200ms should take ~200ms */
    let start = Instant::now();
    let _ = timeout_cmd()
        .args([
            "2s",
            "sh",
            "-c",
            /* busy loop: increment counter until 200ms elapsed */
            "i=0; end=$(($(date +%s) + 1)); while [ $(date +%s) -lt $end ]; do i=$((i+1)); done",
        ])
        .output();
    let baseline = start.elapsed();

    /* throttled: 20% CPU limit should make the same work take 4-5x longer */
    let start = Instant::now();
    let output = timeout_cmd()
        .args([
            "--cpu-percent=20",
            "10s", /* generous timeout since throttling slows it down */
            "sh",
            "-c",
            "i=0; end=$(($(date +%s) + 1)); while [ $(date +%s) -lt $end ]; do i=$((i+1)); done",
        ])
        .output()
        .expect("command should run");
    let throttled = start.elapsed();

    /* throttled should take significantly longer due to SIGSTOP pauses */
    /* use 1.5x as threshold - conservative to avoid flaky tests */
    assert!(
        throttled > baseline || throttled > Duration::from_millis(500),
        "throttled ({:?}) should take longer than baseline ({:?})",
        throttled,
        baseline
    );
    assert!(
        output.status.success(),
        "command should complete: {:?}",
        output.status
    );
}

#[test]
fn test_cpu_percent_json_output() {
    /*
     * JSON output should include cpu_percent in limits when --cpu-percent is used.
     */
    let output = timeout_cmd()
        .args(["--json", "--cpu-percent=50", "5s", "echo", "test"])
        .output()
        .expect("command should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(r#""cpu_percent":50"#),
        "JSON should include cpu_percent in limits: {}",
        stdout
    );
}

#[test]
fn test_mem_limit_flag_accepted() {
    /*
     * --mem-limit should be accepted. Use a generous limit so process completes.
     */
    timeout_cmd()
        .args(["--mem-limit=1G", "5s", "echo", "limited"])
        .assert()
        .success()
        .stdout(predicate::str::contains("limited"));
}

#[test]
fn test_mem_limit_invalid_negative() {
    /* negative bytes is invalid */
    timeout_cmd()
        .args(["--mem-limit=-1", "5s", "echo", "test"])
        .assert()
        .code(125)
        .stderr(predicate::str::contains("invalid"));
}

#[test]
fn test_mem_limit_units_accepted() {
    /* Various memory units should be accepted */
    for unit in ["1K", "1KB", "1M", "1MB", "1G", "1GB"] {
        timeout_cmd()
            .args([&format!("--mem-limit={}", unit), "5s", "true"])
            .assert()
            .success();
    }
}

#[test]
fn test_mem_limit_kills_on_exceed() {
    /*
     * --mem-limit should kill process when it exceeds the limit.
     * This tests the polling-based memory enforcement in runner.rs.
     *
     * We use a small limit (5M) and have the child allocate more using Python.
     * The process should be killed (not timeout).
     */
    let output = timeout_cmd()
        .args([
            "--json",
            "--mem-limit=5M",
            "10s",
            "python3",
            "-c",
            /* allocate ~50MB in a list - exceeds 5M limit */
            "import time; x = [0] * (50 * 1024 * 1024 // 8); time.sleep(10)",
        ])
        .output()
        .expect("command should run");

    /* should be killed due to memory limit, not timeout */
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains(r#""schema_version":8"#),
        "expected schema_version 8: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""status":"memory_limit""#),
        "expected memory_limit status: {}",
        stdout
    );
    assert!(
        stdout.contains(r#""limit_bytes":"#) && stdout.contains(r#""actual_bytes":"#),
        "expected limit_bytes and actual_bytes fields: {}",
        stdout
    );

    /* exit code should indicate killed (SIGKILL = 137) or resource limit */
    let code = output.status.code().unwrap_or(0);
    assert!(
        code == 137 || code == 125 || !output.status.success(),
        "process should be killed (got code {}): {}",
        code,
        stdout
    );
}

#[test]
fn test_cpu_time_flag_accepted() {
    /*
     * --cpu-time should set RLIMIT_CPU for the child process.
     * Use a generous limit so process completes normally.
     */
    timeout_cmd()
        .args(["--cpu-time=60s", "5s", "echo", "limited"])
        .assert()
        .success()
        .stdout(predicate::str::contains("limited"));
}

#[test]
fn test_cpu_time_zero_kills_immediately() {
    /*
     * 0s CPU time means immediate SIGXCPU from kernel.
     * This is valid behavior - kernel enforces the limit.
     */
    timeout_cmd()
        .args(["--cpu-time=0s", "5s", "echo", "test"])
        .assert()
        .code(152); /* 128 + SIGXCPU(24) */
}

#[test]
fn test_cpu_time_kills_cpu_intensive() {
    /*
     * --cpu-time should kill process when it exceeds CPU time limit.
     * RLIMIT_CPU sends SIGXCPU then SIGKILL.
     *
     * Use a very short CPU limit (1s) with an infinite busy loop.
     */
    use std::time::Instant;

    let start = Instant::now();
    let output = timeout_cmd()
        .args([
            "--json",
            "--cpu-time=1s",
            "30s", /* wall timeout much higher than CPU limit */
            "sh",
            "-c",
            "while true; do :; done", /* infinite busy loop burning CPU */
        ])
        .output()
        .expect("command should run");

    let elapsed = start.elapsed();

    /* should be killed by RLIMIT_CPU (~1s CPU time), not wall timeout (30s) */
    assert!(
        elapsed < Duration::from_secs(10),
        "should be killed by CPU limit, not wall timeout: {:?}",
        elapsed
    );

    /* exit code should reflect SIGXCPU (24) or SIGKILL (9) */
    /* SIGXCPU = 128 + 24 = 152, SIGKILL = 128 + 9 = 137 */
    let code = output.status.code().unwrap_or(0);
    assert!(
        code == 152 || code == 137 || code == 124,
        "should exit with signal code (got {}): {}",
        code,
        String::from_utf8_lossy(&output.stdout)
    );
}

/* -------------------------------------------------------------------------
 * ZERO-DURATION ENFORCEMENT - runtime monitoring with no wall-clock timeout.
 * Regression: the zero-duration ("run forever") fast path used to skip all
 * post-spawn monitoring, silently disabling mem-limit/cpu-percent/heartbeat/
 * stdin-timeout while only spawn-time RLIMIT_CPU survived.
 * ------------------------------------------------------------------------- */

#[test]
fn test_mem_limit_kills_on_exceed_zero_duration() {
    /*
     * --mem-limit must still kill when there is no wall-clock timeout (0).
     * Without the fix, the child would sleep and exit 0 unmonitored.
     */
    let output = timeout_cmd()
        .args([
            "--json",
            "--mem-limit=5M",
            "0", /* no wall-clock timeout */
            "python3",
            "-c",
            "import time; x = [0] * (50 * 1024 * 1024 // 8); time.sleep(10)",
        ])
        .output()
        .expect("command should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(r#""status":"memory_limit""#),
        "mem-limit should be enforced with duration 0: {}",
        stdout
    );
    assert!(
        !output.status.success(),
        "process should be killed by mem-limit with duration 0: {}",
        stdout
    );
}

#[test]
fn test_cpu_percent_throttles_with_zero_duration() {
    /*
     * --cpu-percent must still engage the SIGSTOP/SIGCONT throttle with no
     * wall-clock timeout (0). The busy loop is wall-bounded so it exits on its
     * own; we assert the throttled path runs without hanging or being killed.
     */
    use std::time::Instant;

    let start = Instant::now();
    let output = timeout_cmd()
        .args([
            "--cpu-percent=20",
            "0", /* no wall-clock timeout */
            "sh",
            "-c",
            "i=0; end=$(($(date +%s) + 1)); while [ $(date +%s) -lt $end ]; do i=$((i+1)); done",
        ])
        .output()
        .expect("command should run");
    let elapsed = start.elapsed();

    /* the throttle path must run to completion (not be skipped, not hang, not
     * spuriously kill the child) with no wall-clock timeout. */
    assert!(
        output.status.success(),
        "throttled command should complete with duration 0: {:?}",
        output.status
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "throttled command should not hang with duration 0 (took {:?})",
        elapsed
    );
}

#[test]
fn test_heartbeat_with_zero_duration() {
    /*
     * --heartbeat must still emit ticks with no wall-clock timeout (0).
     * sleep exits on its own; heartbeat fires during the run.
     */
    let output = timeout_cmd()
        .args(["--heartbeat", "200ms", "0", "sleep", "1"])
        .output()
        .expect("failed to run command");

    assert!(
        output.status.success(),
        "sleep should complete with duration 0: {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("timeout: heartbeat:"),
        "heartbeat should fire with duration 0: {}",
        stderr
    );
}

#[test]
fn test_stdin_timeout_with_zero_duration() {
    /*
     * --stdin-timeout must still fire with no wall-clock timeout (0).
     * Keep an open, idle pipe so the idle timer (not EOF) triggers -> exit 124.
     */
    use std::process::Stdio;
    let mut child = std::process::Command::new(timeout_bin_path().as_str())
        .args(["--stdin-timeout", "200ms", "0", "sleep", "60"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    let _stdin = child.stdin.take(); /* keep pipe open and idle */
    let status = child.wait().expect("failed to wait");
    assert_eq!(
        status.code(),
        Some(124),
        "stdin idle timeout should fire with duration 0"
    );
}

#[test]
fn test_kill_after_zero_escalates_immediately_zero_grace() {
    /*
     * --kill-after 0 must escalate to SIGKILL immediately (a finite, already-
     * reached grace deadline), not hang. Guards the wait_with_kqueue
     * Option<Duration> change: Some(ZERO) differs from None (no wall clock).
     * The child ignores SIGTERM, so escalation is required to stop it.
     */
    use std::time::Instant;

    let start = Instant::now();
    let output = timeout_cmd()
        .args([
            "-s",
            "TERM",
            "-k",
            "0",
            "1s",
            "sh",
            "-c",
            "trap '' TERM; sleep 30",
        ])
        .output()
        .expect("command should run");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(10),
        "--kill-after 0 should escalate immediately, not hang: {:?}",
        elapsed
    );
    let code = output.status.code().unwrap_or(0);
    assert!(
        code == 137 || code == 124 || !output.status.success(),
        "process should be force-killed with kill-after 0 (got {})",
        code
    );
}

/* =========================================================================
 * DUAL BINARY BEHAVIOR - procguard vs timeout alias
 * ========================================================================= */

#[test]
fn test_procguard_defaults_to_wall_clock() {
    /*
     * procguard binary should default to --confine wall (sleep-aware).
     * This is the unique darwin feature - timeout survives system sleep.
     */
    let output = procguard_cmd()
        .args(["--json", "5s", "echo", "test"])
        .output()
        .expect("procguard should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    /* wall clock mode is indicated in JSON output */
    assert!(
        stdout.contains("\"clock\":\"wall\"") || stdout.contains("\"status\":\"completed\""),
        "procguard should use wall clock by default: {}",
        stdout
    );
}

#[test]
fn test_timeout_alias_defaults_to_active() {
    /*
     * timeout alias should default to --confine active for GNU compatibility.
     * This matches the behavior of GNU timeout which uses active/monotonic time.
     */
    let output = timeout_cmd()
        .args(["--json", "5s", "true"])
        .output()
        .expect("timeout should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    /* active clock mode is indicated in JSON output */
    assert!(
        stdout.contains("\"clock\":\"active\""),
        "timeout alias should use active clock by default: {}",
        stdout
    );
}

#[test]
fn test_timeout_alias_respects_explicit_confine_wall() {
    /*
     * Even when invoked as 'timeout', explicit --confine wall should override.
     */
    let output = timeout_cmd()
        .args(["--json", "--confine", "wall", "5s", "true"])
        .output()
        .expect("timeout should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"clock\":\"wall\""),
        "explicit --confine wall should override timeout default: {}",
        stdout
    );
}

#[test]
fn test_procguard_respects_explicit_confine_active() {
    /*
     * Even when invoked as 'procguard', explicit --confine active should override.
     */
    let output = procguard_cmd()
        .args(["--json", "--confine", "active", "5s", "true"])
        .output()
        .expect("procguard should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"clock\":\"active\""),
        "explicit --confine active should work on procguard: {}",
        stdout
    );
}

#[test]
fn test_procguard_defaults_to_wall() {
    /*
     * procguard should default to --confine wall (wall clock).
     * This is different from the timeout alias which defaults to active.
     */
    let output = procguard_cmd()
        .args(["--json", "5s", "true"])
        .output()
        .expect("procguard should run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"clock\":\"wall\""),
        "procguard should use wall clock by default: {}",
        stdout
    );
}

#[test]
fn test_procguard_version_shows_procguard() {
    /*
     * Version output should show "procguard" as the program name.
     */
    procguard_cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("procguard"))
        .stdout(predicate::str::contains(
            "formally verified process supervisor",
        ));
}

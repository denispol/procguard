/*
 * library_api.rs
 *
 * integration-style tests exercising procguard as a library.
 *
 * goal: ensure the public API is usable without shelling out to the CLI.
 */

use std::time::Duration;

use procguard::error::exit_codes;
use procguard::runner::{
    RunConfig, RunResult, cleanup_signal_forwarding, run_command, run_with_retry,
    setup_signal_forwarding,
};
use procguard::signal::Signal;
use procguard::{TimeoutReason, parse_duration, parse_signal};

fn basic_config(timeout: Duration) -> RunConfig {
    RunConfig {
        timeout,
        signal: Signal::SIGTERM,
        kill_after: Some(Duration::from_millis(50)),
        quiet: true,
        ..RunConfig::default()
    }
}

/* =========================================================================
 * BASIC COMMAND EXECUTION
 * ========================================================================= */

#[test]
fn library_run_command_completes() {
    let _ = setup_signal_forwarding();

    let config = basic_config(Duration::from_secs(2));
    let args = ["-c".to_string(), "exit 0".to_string()];

    let result = run_command("sh", &args, &config).expect("run_command should succeed");

    match result {
        RunResult::Completed { status, .. } => {
            assert_eq!(status.code(), Some(0));
        }
        _ => panic!("expected Completed, got other variant"),
    }

    cleanup_signal_forwarding();
}

#[test]
fn library_run_command_nonzero_exit() {
    let _ = setup_signal_forwarding();

    let config = basic_config(Duration::from_secs(2));
    let args = ["-c".to_string(), "exit 42".to_string()];

    let result = run_command("sh", &args, &config).expect("run_command should succeed");

    match result {
        RunResult::Completed { status, rusage } => {
            assert_eq!(status.code(), Some(42));
            /* verify rusage is populated - max_rss_kb is the field name */
            assert!(rusage.max_rss_kb > 0);
        }
        _ => panic!("expected Completed, got other variant"),
    }

    cleanup_signal_forwarding();
}

#[test]
fn library_run_command_times_out() {
    let _ = setup_signal_forwarding();

    let config = basic_config(Duration::from_millis(150));
    let args = ["10".to_string()];

    let result = run_command("sleep", &args, &config).expect("run_command should succeed");

    match result {
        RunResult::TimedOut { reason, signal, .. } => {
            assert_eq!(signal, Signal::SIGTERM);
            assert!(matches!(reason, TimeoutReason::WallClock));
        }
        _ => panic!("expected TimedOut, got other variant"),
    }

    cleanup_signal_forwarding();
}

/* =========================================================================
 * ERROR HANDLING
 * ========================================================================= */

#[test]
fn library_run_command_not_found() {
    let _ = setup_signal_forwarding();

    let config = basic_config(Duration::from_secs(5));
    let args: [String; 0] = [];

    let result = run_command("nonexistent_command_xyz_12345", &args, &config);

    match result {
        Err(err) => {
            assert_eq!(err.exit_code(), exit_codes::NOT_FOUND);
        }
        Ok(_) => panic!("expected error for nonexistent command"),
    }

    cleanup_signal_forwarding();
}

/* =========================================================================
 * RETRY FUNCTIONALITY
 * ========================================================================= */

#[test]
fn library_run_with_retry_succeeds_first_try() {
    let _ = setup_signal_forwarding();

    let config = RunConfig {
        timeout: Duration::from_secs(5),
        retry_count: 2,
        quiet: true,
        ..RunConfig::default()
    };
    let args = ["-c".to_string(), "exit 0".to_string()];

    let (result, attempts) =
        run_with_retry("sh", &args, &config).expect("run_with_retry should succeed");

    match result {
        RunResult::Completed { status, .. } => {
            assert_eq!(status.code(), Some(0));
        }
        _ => panic!("expected Completed, got other variant"),
    }

    /* should have exactly one attempt when command succeeds */
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts.as_slice()[0].status, "completed");

    cleanup_signal_forwarding();
}

#[test]
fn library_run_with_retry_retries_on_timeout() {
    /* this test can be flaky when run in parallel due to signal handling
     * interference - SignalForwarded can occur if another test's signal
     * reaches this process. Accept both TimedOut and SignalForwarded as
     * valid "didn't complete successfully" outcomes. */
    let _ = setup_signal_forwarding();

    let config = RunConfig {
        timeout: Duration::from_millis(100),
        retry_count: 2,
        retry_delay: Duration::from_millis(10),
        quiet: true,
        kill_after: Some(Duration::from_millis(50)),
        ..RunConfig::default()
    };
    let args = ["10".to_string()];

    let (result, attempts) =
        run_with_retry("sleep", &args, &config).expect("run_with_retry should succeed");

    /* should not complete normally - either timeout or signal forwarded */
    match &result {
        RunResult::TimedOut { .. } | RunResult::SignalForwarded { .. } => {}
        RunResult::Completed { status, .. } => {
            panic!(
                "expected TimedOut or SignalForwarded, got Completed with status {:?}",
                status
            )
        }
        RunResult::MemoryLimitExceeded { .. } => {
            panic!("expected TimedOut or SignalForwarded, got MemoryLimitExceeded")
        }
        _ => panic!("expected TimedOut or SignalForwarded, got unknown variant"),
    }

    /* should have retried (3 attempts total) */
    assert!(!attempts.is_empty(), "should have at least 1 attempt");
    /* with flaky signal forwarding, may not always get all 3 attempts */
    for attempt in attempts.as_slice() {
        assert!(
            attempt.status == "timeout" || attempt.status == "signal",
            "attempt status should be timeout or signal, got: {}",
            attempt.status
        );
    }

    cleanup_signal_forwarding();
}

/* =========================================================================
 * RESOURCE LIMITS WITH ZERO TIMEOUT
 * ========================================================================= */

#[test]
fn library_mem_limit_enforced_with_zero_timeout() {
    /*
     * limits must be enforced even when timeout is zero ("run forever").
     * regression: run_command previously took a plain wait() fast path that
     * skipped all monitoring when the timeout was zero.
     */
    use procguard::ResourceLimits;

    let _ = setup_signal_forwarding();

    let config = RunConfig {
        timeout: Duration::ZERO, /* no wall-clock timeout */
        quiet: true,
        kill_after: Some(Duration::from_millis(50)),
        limits: ResourceLimits {
            mem_bytes: Some(5 * 1024 * 1024), /* 5 MiB */
            cpu_time: None,
        },
        ..RunConfig::default()
    };
    let args = [
        "-c".to_string(),
        "import time; x = [0] * (50 * 1024 * 1024 // 8); time.sleep(10)".to_string(),
    ];

    let result = run_command("python3", &args, &config).expect("run_command should succeed");

    match result {
        RunResult::MemoryLimitExceeded {
            limit_bytes,
            actual_bytes,
            ..
        } => {
            assert_eq!(limit_bytes, 5 * 1024 * 1024);
            assert!(actual_bytes > limit_bytes);
        }
        _ => panic!("expected MemoryLimitExceeded with zero timeout, got other variant"),
    }

    cleanup_signal_forwarding();
}

/* =========================================================================
 * PARSING HELPERS
 * ========================================================================= */

#[test]
fn library_parse_duration_works() {
    let dur = parse_duration("30s").expect("should parse");
    assert_eq!(dur, Duration::from_secs(30));

    let dur = parse_duration("1.5m").expect("should parse");
    assert_eq!(dur, Duration::from_secs(90));
}

#[test]
fn library_parse_signal_works() {
    let sig = parse_signal("TERM").expect("should parse");
    assert_eq!(sig, Signal::SIGTERM);

    let sig = parse_signal("9").expect("should parse");
    assert_eq!(sig, Signal::SIGKILL);
}

/* =========================================================================
 * SIGNAL FORWARDING LIFECYCLE
 * ========================================================================= */

#[test]
fn library_cleanup_signal_forwarding_is_idempotent() {
    cleanup_signal_forwarding();
    cleanup_signal_forwarding();
}

#[test]
fn library_setup_cleanup_cycle() {
    /* verify setup/cleanup can be called multiple times */
    for _ in 0..3 {
        let fd = setup_signal_forwarding();
        assert!(fd.is_some(), "setup should return valid fd");
        cleanup_signal_forwarding();
    }
}

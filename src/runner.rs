/*
 * runner.rs
 *
 * Spawn child, watch clock, kill if needed. All the tricky bits live here.
 *
 * kqueue: we tell the kernel "wake me when the process exits or the timer
 * fires" and then sleep. Zero CPU while waiting. Polling would be dumb.
 *
 * mach_continuous_time: the old mach_absolute_time stops when your laptop
 * sleeps. So a 1 hour timeout could take 8 hours if you close the lid.
 * mach_continuous_time keeps counting through sleep. That's what people expect.
 *
 * Process groups: when you timeout a shell script that spawns children, you
 * want to kill all of them, not just the shell. setpgid + killpg handles that.
 * --foreground disables this for interactive stuff.
 *
 * Signal forwarding: if timeout gets SIGTERM (docker stop, system shutdown),
 * we forward it to the child before dying. Otherwise you get orphans.
 * Self-pipe trick: handler writes to pipe, kqueue watches it.
 */

use alloc::format;
use alloc::string::{String, ToString};
use core::sync::atomic::{AtomicI32, Ordering};
use core::time::Duration;

use crate::args::{Confine, OwnedArgs};
use crate::duration::{is_no_timeout, parse_duration};
use crate::error::{Result, TimeoutError, exit_codes};
use crate::process::{
    RawChild, RawExitStatus, ResourceUsage, SpawnError, spawn_command, spawn_command_with_limits,
};
use crate::rlimit::{ResourceLimits, parse_cpu_percent, parse_cpu_time, parse_mem_limit};
use crate::signal::{Signal, parse_signal, signal_name, signal_number};
use crate::sync::AtomicOnce;
use crate::throttle::{CpuThrottleConfig, CpuThrottleState};
use crate::time_math::{
    advance_ns, deadline_reached, elapsed_ns, remaining_ns, time_to_idle_timeout,
};
use crate::wait::kqueue_delay;

type RawFd = i32;

/*
 * Self-pipe trick for signal forwarding.
 *
 * Problem: we're blocked in kevent() waiting. If we get SIGTERM, we need to
 * forward it to the child. But signal handlers can only call write() and
 * _exit(), so we can't do much from the handler.
 *
 * Fix: create a pipe, handler writes a byte, kqueue watches the read end.
 * Signal arrives, pipe becomes readable, kevent returns. Simple.
 *
 * signalfd would work but that's Linux only. EVFILT_SIGNAL exists but doesn't
 * play nice with process monitoring. Pipe works everywhere.
 *
 * We forward SIGTERM, SIGINT, SIGHUP. The "please die" signals.
 */

/* Read end of signal pipe, -1 if not set. Can be reset by cleanup. */
static SIGNAL_PIPE: AtomicI32 = AtomicI32::new(-1);

/// Set up signal handlers for forwarding signals to child process.
///
/// Handles: SIGTERM, SIGINT, SIGHUP, SIGQUIT, SIGUSR1, SIGUSR2.
/// Returns fd that becomes readable when signal arrives. Call before spawn.
///
/// Fds close automatically on exit. For library use (long-running process),
/// call [`cleanup_signal_forwarding`] after each run to avoid fd leaks and
/// enable subsequent `setup_signal_forwarding` calls.
#[must_use]
pub fn setup_signal_forwarding() -> Option<RawFd> {
    /* Create pipe - write end for signal handler, read end for kqueue */
    let mut fds = [0i32; 2];
    // SAFETY: fds is a valid 2-element array, pipe() writes exactly 2 fds
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    let read_fd = fds[0];
    let write_fd = fds[1];

    // SAFETY: read_fd and write_fd are valid fds just returned by pipe().
    // fcntl with F_GETFL/F_SETFL/F_SETFD are safe operations on valid fds.
    // Multiple ops share the same invariant (fd validity).
    #[allow(clippy::multiple_unsafe_ops_per_block)]
    unsafe {
        /* non-blocking mode is required - signal handler must not block */
        let flags = libc::fcntl(read_fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            libc::close(read_fd);
            libc::close(write_fd);
            return None;
        }
        let write_flags = libc::fcntl(write_fd, libc::F_GETFL);
        if write_flags < 0
            || libc::fcntl(write_fd, libc::F_SETFL, write_flags | libc::O_NONBLOCK) < 0
        {
            libc::close(read_fd);
            libc::close(write_fd);
            return None;
        }
        /* CLOEXEC is best-effort - fd leak to child is harmless */
        let _ = libc::fcntl(read_fd, libc::F_SETFD, libc::FD_CLOEXEC);
        let _ = libc::fcntl(write_fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }

    /* Try to claim the signal pipe slot with CAS. -1 means not set. */
    if SIGNAL_PIPE
        .compare_exchange(-1, read_fd, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        /* Already set (re-entry) - close fds to avoid leak */
        // SAFETY: read_fd and write_fd are valid fds from pipe() above.
        // Both close calls share the same invariant (fd validity from pipe()).
        #[allow(clippy::multiple_unsafe_ops_per_block)]
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        let existing = SIGNAL_PIPE.load(Ordering::SeqCst);
        return if existing >= 0 { Some(existing) } else { None };
    }

    /* Store write_fd BEFORE registering signal handlers to prevent race.
     * If a signal arrives between sigaction() and store(), the handler would
     * see -1 and drop the signal. Store first so handler always has valid fd. */
    SIGNAL_WRITE_FD.store(write_fd, Ordering::SeqCst);

    // SAFETY: sigaction struct is zeroed then properly initialized.
    // signal_handler is an extern "C" fn with correct signature.
    // sigemptyset and sigaction are standard POSIX calls with valid args.
    // All ops share the invariant of setting up signal handlers atomically.
    #[allow(clippy::multiple_unsafe_ops_per_block)]
    unsafe {
        let mut sa: libc::sigaction = core::mem::zeroed();
        sa.sa_sigaction = signal_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&raw mut sa.sa_mask);

        libc::sigaction(libc::SIGTERM, &sa, core::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, core::ptr::null_mut());
        libc::sigaction(libc::SIGHUP, &sa, core::ptr::null_mut());
        libc::sigaction(libc::SIGQUIT, &sa, core::ptr::null_mut());
        libc::sigaction(libc::SIGUSR1, &sa, core::ptr::null_mut());
        libc::sigaction(libc::SIGUSR2, &sa, core::ptr::null_mut());
    }

    Some(read_fd)
}

/* Global write fd for signal handler (atomic for signal safety) */
static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Close signal pipe fds and reset handlers to default.
///
/// Not needed for CLI (fds close on exit). For library use where you call
/// `run_command` multiple times, this prevents fd leaks and resets state
/// so `setup_signal_forwarding` can be called again.
///
/// Don't call while another thread is inside `run_command`.
pub fn cleanup_signal_forwarding() {
    /* Reset signal handlers FIRST to prevent writes during fd cleanup.
     * Otherwise a signal arriving between swap(-1) and close() could
     * write to a closed fd (harmless EBADF) or worse, a reused fd. */
    // SAFETY: SIG_DFL is the standard default handler, sigaction is safe with valid args.
    // All ops share the invariant of resetting signal handlers atomically.
    #[allow(clippy::multiple_unsafe_ops_per_block)]
    unsafe {
        let mut sa: libc::sigaction = core::mem::zeroed();
        sa.sa_sigaction = libc::SIG_DFL;
        sa.sa_flags = 0;
        libc::sigemptyset(&raw mut sa.sa_mask);

        libc::sigaction(libc::SIGTERM, &sa, core::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, core::ptr::null_mut());
        libc::sigaction(libc::SIGHUP, &sa, core::ptr::null_mut());
        libc::sigaction(libc::SIGQUIT, &sa, core::ptr::null_mut());
        libc::sigaction(libc::SIGUSR1, &sa, core::ptr::null_mut());
        libc::sigaction(libc::SIGUSR2, &sa, core::ptr::null_mut());
    }

    /* Now safe to close fds - no signal handler will write to them */
    let write_fd = SIGNAL_WRITE_FD.swap(-1, Ordering::SeqCst);
    if write_fd >= 0 {
        // SAFETY: write_fd was set by setup_signal_forwarding and is valid.
        unsafe {
            libc::close(write_fd);
        }
    }

    /* Close read fd and reset SIGNAL_PIPE to -1 so setup can be called again */
    let read_fd = SIGNAL_PIPE.swap(-1, Ordering::SeqCst);
    if read_fd >= 0 {
        // SAFETY: read_fd was set by setup_signal_forwarding and is valid.
        unsafe {
            libc::close(read_fd);
        }
    }
}

/* Minimal signal handler - write the signal number to the pipe */
extern "C" fn signal_handler(sig: i32) {
    let fd = SIGNAL_WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        // SAFETY: fd was validated >= 0 and set by setup_signal_forwarding().
        // write() with a 1-byte buffer is async-signal-safe per POSIX.
        // We ignore errors since we can't do anything useful in a signal handler.
        // Store actual signal number (SIGTERM=15, SIGINT=2, SIGHUP=1 all fit in u8).
        unsafe {
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let byte: u8 = sig as u8;
            let _ = libc::write(fd, (&raw const byte).cast(), 1);
        }
    }
}

/* check if signal pipe has data (signal was received) */
fn read_signal_from_pipe(fd: RawFd) -> Option<Signal> {
    let mut buf = [0u8; 1];
    // SAFETY: buf is a valid 1-byte buffer, fd is the read end of our pipe.
    // read() will return -1 with EAGAIN if no data (non-blocking fd).
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), 1) };
    if n > 0 {
        /* Decode the signal number written by signal_handler */
        Signal::try_from_raw(buf[0] as i32).or(Some(Signal::SIGTERM))
    } else {
        None
    }
}

/*
 * Timing APIs - two modes based on --confine flag:
 *
 * Wall mode (default): mach_continuous_time()
 *   - Continues counting during system sleep
 *   - A 1-hour timeout fires when you open the lid after 7 hours of sleep
 *   - This is what most users expect from a timeout
 *
 * Active mode: clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW)
 *   - Only counts awake/active time, pauses during sleep
 *   - ~28% faster (no timebase conversion needed)
 *   - Useful for benchmarks where idle time shouldn't count
 *
 * Empirically tested: CLOCK_MONOTONIC_RAW does NOT advance during pmset sleepnow.
 * See tests/clock_api_comparison.rs for benchmarks and verification.
 */

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_continuous_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    fn clock_gettime_nsec_np(clock_id: libc::clockid_t) -> u64;
}

const CLOCK_MONOTONIC_RAW: libc::clockid_t = 4;

/* get timebase ratio, cached forever. returns Err if denom is zero (invalid FFI data). */
fn get_timebase_info() -> Result<(u64, u64)> {
    static TIMEBASE: AtomicOnce<(u64, u64)> = AtomicOnce::new();

    /* check if already cached */
    if let Some(&cached) = TIMEBASE.get() {
        return Ok(cached);
    }

    /* fetch from FFI */
    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: info is a valid MachTimebaseInfo struct with correct layout (#[repr(C)]).
    // mach_timebase_info always succeeds on macOS and fills in numer/denom.
    unsafe {
        mach_timebase_info(&raw mut info);
    }

    /* validate FFI data - denom == 0 would cause division by zero */
    if info.denom == 0 {
        return Err(TimeoutError::TimebaseError);
    }

    let result = (u64::from(info.numer), u64::from(info.denom));
    /* cache only on success */
    let _ = TIMEBASE.set(result);
    Ok(result)
}

/* Wall time in nanoseconds - includes system sleep (mach_continuous_time).
 *
 * # Errors
 * Returns `TimebaseError` if mach_timebase_info returned invalid data (zero denominator).
 */
#[inline]
fn wall_now_ns() -> Result<u64> {
    let (numer, denom) = get_timebase_info()?;
    // SAFETY: mach_continuous_time() has no preconditions, always returns valid u64.
    let abs_time = unsafe { mach_continuous_time() };

    /* Apple Silicon: numer == denom == 1, fast path avoids division */
    if numer == denom {
        return Ok(abs_time);
    }

    /* Intel: need conversion. u128 intermediate avoids overflow.
     * Use checked_div as defense-in-depth (denom already validated above). */
    let intermediate = u128::from(abs_time) * u128::from(numer);
    let result = intermediate
        .checked_div(u128::from(denom))
        .ok_or(TimeoutError::TimebaseError)?;

    #[allow(clippy::cast_possible_truncation)]
    Ok(result as u64)
}

/* Active time in nanoseconds - excludes system sleep (CLOCK_MONOTONIC_RAW) */
#[inline]
fn active_now_ns() -> u64 {
    // SAFETY: clock_gettime_nsec_np with valid clock_id always succeeds on macOS
    unsafe { clock_gettime_nsec_np(CLOCK_MONOTONIC_RAW) }
}

/* Get current time based on confine mode.
 *
 * # Errors
 * Returns `TimebaseError` if wall mode and mach_timebase_info returned invalid data.
 */
#[inline]
fn precise_now_ns(confine: Confine) -> Result<u64> {
    match confine {
        Confine::Wall => wall_now_ns(),
        Confine::Active => Ok(active_now_ns()),
    }
}

/* max ns that fits in isize (~292 years on 64-bit) */
const MAX_TIMER_NS: u64 = isize::MAX as u64;

/* duration to ns, clamped for kqueue */
#[inline]
fn duration_to_ns(d: Duration) -> u64 {
    d.as_secs()
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::from(d.subsec_nanos()))
        .min(MAX_TIMER_NS)
}

/* duration to ms as u64 (avoids u128 from as_millis()) */
#[inline]
fn duration_ms(d: Duration) -> u64 {
    d.as_secs()
        .saturating_mul(1000)
        .saturating_add(u64::from(d.subsec_millis()))
}

/* what happened when we ran the on-timeout hook */
#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Default)]
pub struct HookResult {
    pub ran: bool,              /* did we actually run it? */
    pub exit_code: Option<i32>, /* None if timed out or failed to start */
    pub timed_out: bool,        /* killed because it took too long? */
    pub elapsed_ms: u64,        /* how long it ran */
}

/* result of a single attempt in retry mode */
#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy, Default)]
pub struct AttemptResult {
    pub status: &'static str,   /* "completed", "timeout", "error" */
    pub exit_code: Option<i32>, /* exit code if completed */
    pub elapsed_ms: u64,        /* how long this attempt took */
}

/* fixed-size array of attempt results - avoids Vec allocation overhead */
pub const MAX_RETRIES: usize = 32;

pub struct Attempts {
    data: [AttemptResult; MAX_RETRIES],
    len: usize,
}

impl Default for Attempts {
    fn default() -> Self {
        Self::new()
    }
}

impl Attempts {
    #[inline]
    pub fn new() -> Self {
        Self {
            data: [AttemptResult::default(); MAX_RETRIES],
            len: 0,
        }
    }

    #[inline]
    pub fn push(&mut self, item: AttemptResult) {
        if self.len < MAX_RETRIES {
            self.data[self.len] = item;
            self.len += 1;
        }
    }

    #[inline]
    pub fn as_slice(&self) -> &[AttemptResult] {
        &self.data[..self.len]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Result of running a command with timeout.
///
/// This enum represents all possible outcomes when running a command:
/// - `Completed`: command finished before timeout
/// - `TimedOut`: command exceeded time limit
/// - `MemoryLimitExceeded`: command exceeded memory limit
/// - `SignalForwarded`: parent received a signal and forwarded it
#[cfg_attr(test, derive(Debug))]
#[non_exhaustive]
pub enum RunResult {
    Completed {
        status: RawExitStatus,
        rusage: ResourceUsage,
    },
    TimedOut {
        signal: Signal,
        killed: bool, /* true if we had to escalate to SIGKILL */
        status: Option<RawExitStatus>,
        rusage: Option<ResourceUsage>,
        hook: Option<HookResult>, /* on-timeout hook result if configured */
        reason: TimeoutReason,    /* what triggered the timeout */
    },
    MemoryLimitExceeded {
        signal: Signal,
        killed: bool,
        status: Option<RawExitStatus>,
        rusage: Option<ResourceUsage>,
        limit_bytes: u64,  /* the limit that was exceeded */
        actual_bytes: u64, /* memory usage when limit was hit */
    },
    SignalForwarded {
        /* we got SIGTERM/SIGINT/SIGHUP, passed it on */
        signal: Signal,
        status: Option<RawExitStatus>,
        rusage: Option<ResourceUsage>,
    },
}

pub struct ThrottleContext {
    pub cfg: CpuThrottleConfig,
    pub state: CpuThrottleState,
}

/* memory limit enforcement config */
struct MemoryLimitConfig {
    limit_bytes: u64,
    check_interval_ns: u64,
}

/// Reason for timeout (wall clock vs stdin idle)
#[cfg_attr(test, derive(Debug))]
#[derive(Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TimeoutReason {
    #[default]
    WallClock,
    StdinIdle,
}

impl RunResult {
    /* what exit code to return per GNU spec */
    #[must_use]
    pub fn exit_code(&self, preserve_status: bool, timeout_exit_code: u8) -> u8 {
        match self {
            Self::Completed { status, .. } => status_to_exit_code(status),
            Self::TimedOut {
                signal,
                killed,
                status,
                hook: _,
                rusage: _,
                reason: _,
            } => {
                if preserve_status {
                    status.map_or_else(
                        || {
                            let sig = if *killed { Signal::SIGKILL } else { *signal };
                            signal_exit_code(sig)
                        },
                        |s| status_to_exit_code(&s),
                    )
                } else {
                    timeout_exit_code
                }
            }
            Self::MemoryLimitExceeded {
                signal,
                killed,
                status,
                ..
            } => {
                /* same as timeout - memory limit is a resource limit timeout */
                if preserve_status {
                    status.map_or_else(
                        || {
                            let sig = if *killed { Signal::SIGKILL } else { *signal };
                            signal_exit_code(sig)
                        },
                        |s| status_to_exit_code(&s),
                    )
                } else {
                    timeout_exit_code
                }
            }
            Self::SignalForwarded { signal, status, .. } => {
                /* We got killed by a signal - return 128 + signum like the child would */
                status.map_or_else(|| signal_exit_code(*signal), |s| status_to_exit_code(&s))
            }
        }
    }

    /* get resource usage if available */
    #[must_use]
    pub fn resource_usage(&self) -> Option<&ResourceUsage> {
        match self {
            Self::Completed { rusage, .. } => Some(rusage),
            Self::TimedOut { rusage, .. } => rusage.as_ref(),
            Self::MemoryLimitExceeded { rusage, .. } => rusage.as_ref(),
            Self::SignalForwarded { rusage, .. } => rusage.as_ref(),
        }
    }
}

/* POSIX: exit_code = 128 + signum */
#[inline]
#[allow(clippy::cast_sign_loss)]
const fn signal_exit_code(signal: Signal) -> u8 {
    ((128i32 + signal_number(signal)) & 0xFF) as u8
}

/* exit status to 8-bit code, POSIX style */
#[allow(clippy::cast_sign_loss)]
fn status_to_exit_code(status: &RawExitStatus) -> u8 {
    if let Some(sig) = status.signal() {
        return ((128i32 + sig) & 0xFF) as u8;
    }

    (status.code().unwrap_or(1) & 0xFF) as u8
}

/// Configuration for running a command with timeout.
///
/// Construct via struct literal or use [`RunConfig::default()`] as a base.
/// All durations use wall-clock time by default (survives system sleep).
///
/// # Example
///
/// ```ignore
/// use procguard::{RunConfig, Signal};
/// use std::time::Duration;
///
/// let config = RunConfig {
///     timeout: Duration::from_secs(30),
///     signal: Signal::SIGTERM,
///     ..RunConfig::default()
/// };
/// ```
///
/// # Stability Note
///
/// New fields may be added in minor versions. Use `..RunConfig::default()`
/// to ensure forward compatibility.
pub struct RunConfig {
    /// Maximum time before sending the timeout signal.
    pub timeout: Duration,
    /// Signal to send on timeout (default: SIGTERM).
    pub signal: Signal,
    /// Grace period before escalating to SIGKILL. `None` = no escalation.
    pub kill_after: Option<Duration>,
    /// If `true`, don't create a process group (child inherits parent's group).
    pub foreground: bool,
    /// Print signal diagnostics to stderr.
    pub verbose: bool,
    /// Suppress timeout's own error messages.
    pub quiet: bool,
    /// Exit code when command times out (default: 124).
    pub timeout_exit_code: u8,
    /// Shell command to run before killing on timeout. `%p` is replaced with child PID.
    pub on_timeout: Option<String>,
    /// Time limit for the `on_timeout` hook (default: 5s).
    pub on_timeout_limit: Duration,
    /// Time mode: `Wall` (includes sleep) or `Active` (excludes sleep).
    pub confine: Confine,
    /// Number of retries on timeout (0 = no retry).
    pub retry_count: u32,
    /// Delay between retries.
    pub retry_delay: Duration,
    /// Multiplier for delay each retry (1 = no backoff, 2 = exponential).
    pub retry_backoff: u32,
    /// Print heartbeat status to stderr at this interval.
    pub heartbeat: Option<Duration>,
    /// Timeout if stdin has no activity for this duration.
    pub stdin_timeout: Option<Duration>,
    /// If `true`, detect stdin idle without consuming data (use with `stdin_timeout`).
    pub stdin_passthrough: bool,
    /// Resource limits (memory, CPU time).
    pub limits: ResourceLimits,
    /// CPU throttling configuration.
    pub cpu_throttle: Option<CpuThrottleConfig>,
}

impl Default for RunConfig {
    /// Returns a default configuration with 30-second timeout and SIGTERM.
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            signal: Signal::SIGTERM,
            kill_after: None,
            foreground: false,
            verbose: false,
            quiet: false,
            timeout_exit_code: crate::error::exit_codes::TIMEOUT,
            on_timeout: None,
            on_timeout_limit: Duration::from_secs(5),
            confine: Confine::Wall,
            retry_count: 0,
            retry_delay: Duration::ZERO,
            retry_backoff: 1,
            heartbeat: None,
            stdin_timeout: None,
            stdin_passthrough: false,
            limits: ResourceLimits::default(),
            cpu_throttle: None,
        }
    }
}

impl RunConfig {
    /* build config from CLI args. fails if duration/signal is bogus. */
    pub fn from_args(args: &OwnedArgs, duration_str: &str) -> Result<Self> {
        let timeout = parse_duration(duration_str)?;
        let signal = parse_signal(&args.signal)?;
        let kill_after = args
            .kill_after
            .as_ref()
            .map(|s| parse_duration(s))
            .transpose()?;
        let on_timeout_limit = parse_duration(&args.on_timeout_limit)?;

        /* parse retry options */
        let retry_count = args
            .retry
            .as_ref()
            .map(|s| {
                s.parse::<u32>()
                    .map_err(|_| TimeoutError::Internal(format!("invalid retry count: '{}'", s)))
            })
            .transpose()?
            .unwrap_or(0);

        let retry_delay = args
            .retry_delay
            .as_ref()
            .map(|s| parse_duration(s))
            .transpose()?
            .unwrap_or(Duration::ZERO);

        /* parse backoff multiplier - strip 'x' suffix if present */
        /* ensure >= 1 to prevent 0^n = 0 zeroing all delays */
        let retry_backoff = args
            .retry_backoff
            .as_ref()
            .map(|s| {
                let s = s.trim_end_matches('x').trim_end_matches('X');
                s.parse::<u32>().map_err(|_| {
                    TimeoutError::Internal(format!(
                        "invalid retry backoff: '{}' (use e.g., 2x)",
                        args.retry_backoff.as_ref().unwrap()
                    ))
                })
            })
            .transpose()?
            .unwrap_or(1)
            .max(1);

        /* parse heartbeat interval */
        let heartbeat = args
            .heartbeat
            .as_ref()
            .map(|s| parse_duration(s))
            .transpose()?;

        /* parse stdin idle timeout */
        let stdin_timeout = args
            .stdin_timeout
            .as_ref()
            .map(|s| parse_duration(s))
            .transpose()?;

        /* parse resource limits */
        let mem_limit = args
            .mem_limit
            .as_ref()
            .map(|s| parse_mem_limit(s))
            .transpose()?;

        let cpu_time = args
            .cpu_time
            .as_ref()
            .map(|s| parse_cpu_time(s))
            .transpose()?;

        let limits = ResourceLimits {
            mem_bytes: mem_limit,
            cpu_time,
        };

        /* parse CPU throttle percent */
        let cpu_throttle = args
            .cpu_percent
            .as_ref()
            .map(|s| parse_cpu_percent(s))
            .transpose()?;

        /* warn if cpu-percent is very low - may cause stuttery execution */
        if let Some(ref pct) = cpu_throttle
            && pct.get() < 10
            && !args.quiet
        {
            crate::eprintln!(
                "warning: --cpu-percent {} is very low; may cause stuttery execution",
                pct.get()
            );
        }

        let cpu_throttle = cpu_throttle.map(|percent| CpuThrottleConfig {
            percent,
            interval_ns: duration_to_ns(Duration::from_millis(100)),
            sleep_ns: duration_to_ns(Duration::from_millis(50)),
        });

        if args.stdin_passthrough && stdin_timeout.is_none() {
            return Err(TimeoutError::Internal(
                "--stdin-passthrough requires --stdin-timeout".to_string(),
            ));
        }

        /* Warn if on-timeout-limit exceeds main timeout (when hook is set) */
        if args.on_timeout.is_some() && on_timeout_limit > timeout && !is_no_timeout(&timeout) {
            crate::eprintln!(
                "warning: --on-timeout-limit ({}) exceeds main timeout ({})",
                args.on_timeout_limit,
                duration_str
            );
        }

        Ok(Self {
            timeout,
            signal,
            kill_after,
            foreground: args.foreground,
            verbose: args.verbose,
            quiet: args.quiet,
            timeout_exit_code: args.timeout_exit_code.unwrap_or(exit_codes::TIMEOUT),
            on_timeout: args.on_timeout.clone(),
            on_timeout_limit,
            confine: args.confine,
            retry_count,
            retry_delay,
            retry_backoff,
            heartbeat,
            stdin_timeout,
            stdin_passthrough: args.stdin_passthrough,
            limits,
            cpu_throttle,
        })
    }
}

/// Spawn command and enforce timeout.
///
/// Errors: command not found, permission denied, spawn failed, signal failed.
pub fn run_command(command: &str, args: &[String], config: &RunConfig) -> Result<RunResult> {
    /* put child in its own process group unless foreground mode */
    let use_process_group = !config.foreground;
    let spawn_result = if config.limits.is_empty() {
        spawn_command(command, args, use_process_group)
    } else {
        spawn_command_with_limits(command, args, use_process_group, &config.limits)
    };

    let mut child = spawn_result.map_err(|e| match e {
        SpawnError::NotFound(s) => TimeoutError::CommandNotFound(s),
        SpawnError::PermissionDenied(s) => TimeoutError::PermissionDenied(s),
        SpawnError::Spawn(errno) => TimeoutError::SpawnError(errno),
        SpawnError::Wait(errno) => TimeoutError::SpawnError(errno),
        SpawnError::InvalidArg => TimeoutError::Internal("invalid argument".to_string()),
    })?;

    /* mem-limit polling, cpu-percent throttle, heartbeat, and stdin-idle are only
     * driven inside monitor_with_timeout. cpu_time is a spawn-time RLIMIT_CPU and
     * needs no monitor, so it is not counted here. */
    let needs_runtime_monitoring = config.limits.mem_bytes.is_some()
        || config.cpu_throttle.is_some()
        || config.heartbeat.is_some()
        || config.stdin_timeout.is_some();

    /* zero timeout = run forever: keep the zero-overhead wait only when there is
     * nothing to monitor; otherwise fall through so limits are still enforced. */
    if is_no_timeout(&config.timeout) && !needs_runtime_monitoring {
        let (status, rusage) = child.wait().map_err(|e| match e {
            SpawnError::Wait(errno) => TimeoutError::SpawnError(errno),
            _ => TimeoutError::Internal("wait failed".to_string()),
        })?;
        return Ok(RunResult::Completed { status, rusage });
    }

    monitor_with_timeout(&mut child, config)
}

/// Run command with retry on timeout.
///
/// Returns the final result and attempt results for JSON output.
/// Only retries on timeout - other failures (exit code, signal) are returned immediately.
/// Max retries capped at MAX_RETRIES (32) to avoid unbounded allocation.
pub fn run_with_retry(
    command: &str,
    args: &[String],
    config: &RunConfig,
) -> Result<(RunResult, Attempts)> {
    /* max_attempts = retry_count + 1 (initial attempt), capped at MAX_RETRIES */
    /* note: --retry=31 gives 32 attempts (max), --retry=32+ also gives 32 */
    let capped_retry = config.retry_count.min(MAX_RETRIES as u32 - 1);
    if config.verbose && !config.quiet && config.retry_count > capped_retry {
        crate::eprintln!(
            "timeout: warning: retry count {} capped to maximum {}",
            config.retry_count,
            MAX_RETRIES - 1
        );
    }
    let max_attempts = capped_retry.saturating_add(1);
    let mut attempts = Attempts::new();
    let signal_fd = {
        let fd = SIGNAL_PIPE.load(Ordering::SeqCst);
        if fd >= 0 { Some(fd) } else { None }
    };

    /* safety counter to prevent infinite loops even if logic has bugs */
    let mut safety_counter: u32 = 0;
    const SAFETY_LIMIT: u32 = MAX_RETRIES as u32 + 10;

    for attempt in 0..max_attempts {
        /* defense-in-depth: abort if we've looped too many times */
        safety_counter += 1;
        if safety_counter > SAFETY_LIMIT {
            return Err(TimeoutError::Internal(
                "retry loop exceeded safety limit".to_string(),
            ));
        }

        let attempt_start = precise_now_ns(config.confine).unwrap_or(0);

        let result = run_command(command, args, config)?;
        /* use checked elapsed - fallback to 0 on clock anomaly */
        let attempt_elapsed_ms = elapsed_ns(
            attempt_start,
            precise_now_ns(config.confine).unwrap_or(attempt_start),
        )
        .unwrap_or(0)
            / 1_000_000;

        match &result {
            RunResult::TimedOut { .. } | RunResult::MemoryLimitExceeded { .. } => {
                attempts.push(AttemptResult {
                    status: "timeout",
                    exit_code: None,
                    elapsed_ms: attempt_elapsed_ms,
                });

                /* check if we should retry */
                let is_last_attempt = attempt + 1 >= max_attempts;
                if is_last_attempt {
                    return Ok((result, attempts));
                }

                /* calculate delay with exponential backoff */
                /* backoff^attempt: attempt 0 gets base delay, attempt 1 gets delay*backoff, etc */
                /* cap at 5 minutes to prevent runaway delays with large backoff values */
                const MAX_DELAY_MS: u64 = 5 * 60 * 1000; /* 5 minutes */
                let delay = if config.retry_backoff > 1 {
                    let multiplier = (config.retry_backoff as u64).saturating_pow(attempt);
                    let delay_ms = duration_ms(config.retry_delay);
                    let total_ms = delay_ms.saturating_mul(multiplier).min(MAX_DELAY_MS);
                    Duration::from_millis(total_ms)
                } else {
                    config.retry_delay
                };

                if config.verbose && !config.quiet {
                    crate::eprintln!(
                        "timeout: attempt {} timed out, retry delay {}ms",
                        attempt + 1,
                        duration_ms(delay)
                    );
                }

                /* wait with kqueue delay, checking for signals */
                if !delay.is_zero() && !kqueue_delay(delay, signal_fd) {
                    /* signal received during delay - abort retries */
                    let sig = signal_fd
                        .and_then(read_signal_from_pipe)
                        .unwrap_or(Signal::SIGTERM); /* defensive fallback */
                    return Ok((
                        RunResult::SignalForwarded {
                            signal: sig,
                            status: None,
                            rusage: None,
                        },
                        attempts,
                    ));
                }
            }
            RunResult::Completed { status, .. } => {
                attempts.push(AttemptResult {
                    status: "completed",
                    exit_code: status.code(),
                    elapsed_ms: attempt_elapsed_ms,
                });
                return Ok((result, attempts));
            }
            RunResult::SignalForwarded { .. } => {
                /* signal forwarded - don't retry */
                attempts.push(AttemptResult {
                    status: "signal_forwarded",
                    exit_code: None,
                    elapsed_ms: attempt_elapsed_ms,
                });
                return Ok((result, attempts));
            }
        }
    }

    /* shouldn't reach here, but just in case */
    Err(TimeoutError::Internal(
        "retry loop exited unexpectedly".to_string(),
    ))
}

/*
 * main timeout logic using kqueue. kernel wakes us on process exit
 * or timer expiry - zero CPU while waiting.
 */
fn monitor_with_timeout(child: &mut RawChild, config: &RunConfig) -> Result<RunResult> {
    #[allow(clippy::cast_possible_wrap)]
    let pid = child.id() as i32;
    let start_ns = precise_now_ns(config.confine)?;

    /* set up CPU throttle state if enabled */
    let mut throttle_ctx = if let Some(cfg) = config.cpu_throttle {
        match CpuThrottleState::new(pid, start_ns) {
            Ok(state) => Some(ThrottleContext { cfg, state }),
            Err(e) => {
                if !config.quiet {
                    crate::eprintln!("timeout: warning: CPU throttle disabled ({})", e);
                }
                None
            }
        }
    } else {
        None
    };

    /* build heartbeat config if enabled */
    let heartbeat_config = config.heartbeat.map(|interval| HeartbeatConfig {
        interval_ns: duration_to_ns(interval),
        quiet: config.quiet,
        pid,
        start_ns,
    });

    /* build stdin timeout config if enabled */
    let stdin_timeout_config = config.stdin_timeout.map(|d| StdinTimeoutConfig {
        timeout_ns: duration_to_ns(d),
        last_activity_ns: start_ns,
        mode: if config.stdin_passthrough {
            StdinMode::Passthrough
        } else {
            StdinMode::Consume
        },
    });

    /* build memory limit config if enabled */
    let memory_limit_config = config
        .limits
        .mem_bytes
        .map(|limit_bytes| MemoryLimitConfig {
            limit_bytes,
            check_interval_ns: duration_to_ns(Duration::from_millis(100)), /* poll every 100ms */
        });

    /* zero duration = run forever: no wall-clock deadline, but still monitor
     * limits/throttle/heartbeat/stdin until the child exits. */
    let wall_timeout = if is_no_timeout(&config.timeout) {
        None
    } else {
        Some(config.timeout)
    };

    /* wait for exit or timeout */
    let exit_result = wait_with_kqueue(
        child,
        pid,
        wall_timeout,
        config.confine,
        heartbeat_config,
        stdin_timeout_config,
        throttle_ctx.as_mut(),
        memory_limit_config,
    )?;

    /* track which timeout triggered */
    let timeout_reason = match &exit_result {
        WaitResult::TimedOut(reason) => *reason,
        _ => TimeoutReason::WallClock, /* default, won't be used */
    };

    match exit_result {
        WaitResult::Exited(status, rusage) => {
            /* process already reaped - mark as exited to prevent PID recycling issues */
            if let Some(ref mut ctx) = throttle_ctx {
                ctx.state.mark_process_exited();
            }
            return Ok(RunResult::Completed { status, rusage });
        }
        WaitResult::ReceivedSignal(sig) => {
            /* We received SIGTERM/SIGINT/SIGHUP - forward to child and exit */
            if config.verbose && !config.quiet {
                crate::eprintln!("timeout: forwarding signal {} to command", signal_name(sig));
            }
            /* resume if throttle had it stopped - prevents deadlock */
            if let Some(ref mut ctx) = throttle_ctx {
                ctx.state.resume();
            }
            send_signal(pid, sig, config.foreground)?;
            /* wait for child - extract rusage even if wait returns error (child exited) */
            let (status, rusage) = match child.wait() {
                Ok((s, r)) => (Some(s), Some(r)),
                Err(_) => (None, None), /* child already reaped or wait failed */
            };
            /* mark process exited to prevent PID recycling issues in Drop */
            if let Some(ref mut ctx) = throttle_ctx {
                ctx.state.mark_process_exited();
            }
            return Ok(RunResult::SignalForwarded {
                signal: sig,
                status,
                rusage,
            });
        }
        WaitResult::MemoryLimitExceeded {
            limit_bytes,
            actual_bytes,
        } => {
            /* memory limit exceeded - kill process and return error */
            if config.verbose && !config.quiet {
                crate::eprintln!(
                    "timeout: memory limit exceeded ({} bytes > {} bytes limit)",
                    actual_bytes,
                    limit_bytes
                );
            }

            /* resume if throttle had it stopped - prevents deadlock */
            if let Some(ref mut ctx) = throttle_ctx {
                ctx.state.resume();
            }

            /* send SIGTERM first */
            send_signal(pid, config.signal, config.foreground)?;

            /* wait for child with kill_after grace period if configured */
            if let Some(kill_after) = config.kill_after {
                /* throttle disabled - process needs to run signal handler.
                 * Some(kill_after) keeps a finite grace: --kill-after 0 escalates
                 * immediately, unlike the None (no-deadline) main wait. */
                let grace_result = wait_with_kqueue(
                    child,
                    pid,
                    Some(kill_after),
                    config.confine,
                    None,
                    None,
                    None, /* throttle disabled during grace period */
                    None, /* no memory limit during grace period */
                )?;

                match grace_result {
                    WaitResult::Exited(status, rusage) => {
                        /* mark process exited to prevent PID recycling issues */
                        if let Some(ref mut ctx) = throttle_ctx {
                            ctx.state.mark_process_exited();
                        }
                        return Ok(RunResult::MemoryLimitExceeded {
                            signal: config.signal,
                            killed: false,
                            status: Some(status),
                            rusage: Some(rusage),
                            limit_bytes,
                            actual_bytes,
                        });
                    }
                    _ => {
                        /* Still alive after grace period - SIGKILL */
                        /* resume if throttle had it stopped - prevents deadlock */
                        if let Some(ref mut ctx) = throttle_ctx {
                            ctx.state.resume();
                        }
                        send_signal(pid, Signal::SIGKILL, config.foreground)?;
                        let (status, rusage) = child.wait().map_err(|e| match e {
                            SpawnError::Wait(errno) => TimeoutError::SpawnError(errno),
                            _ => TimeoutError::Internal("wait failed".to_string()),
                        })?;
                        /* mark process exited to prevent PID recycling issues */
                        if let Some(ref mut ctx) = throttle_ctx {
                            ctx.state.mark_process_exited();
                        }
                        return Ok(RunResult::MemoryLimitExceeded {
                            signal: config.signal,
                            killed: true,
                            status: Some(status),
                            rusage: Some(rusage),
                            limit_bytes,
                            actual_bytes,
                        });
                    }
                }
            } else {
                /* No kill-after, just wait for it to die */
                let (status, rusage) = child.wait().map_err(|e| match e {
                    SpawnError::Wait(errno) => TimeoutError::SpawnError(errno),
                    _ => TimeoutError::Internal("wait failed".to_string()),
                })?;
                /* mark process exited to prevent PID recycling issues */
                if let Some(ref mut ctx) = throttle_ctx {
                    ctx.state.mark_process_exited();
                }
                return Ok(RunResult::MemoryLimitExceeded {
                    signal: config.signal,
                    killed: false,
                    status: Some(status),
                    rusage: Some(rusage),
                    limit_bytes,
                    actual_bytes,
                });
            }
        }
        WaitResult::TimedOut(reason) => {
            /* Continue to timeout handling below, with the reason */
            if config.verbose && !config.quiet {
                let reason_str = match reason {
                    TimeoutReason::WallClock => "wall clock",
                    TimeoutReason::StdinIdle => "stdin idle",
                };
                crate::eprintln!("timeout: triggered by {}", reason_str);
            }
        }
    }

    /* Run on-timeout hook if specified */
    let hook_result = config
        .on_timeout
        .as_ref()
        .map(|cmd| run_on_timeout_hook(cmd, pid, config));

    /* time's up, send the signal */
    if config.verbose && !config.quiet {
        crate::eprintln!(
            "timeout: sending signal {} to command",
            signal_name(config.signal)
        );
    }

    /* resume if throttle had it stopped - prevents deadlock.
     * sending SIGTERM to a SIGSTOP'd process creates deadlock if child
     * intercepts the signal (can't run handler while stopped). */
    if let Some(ref mut ctx) = throttle_ctx {
        ctx.state.resume();
    }

    send_signal(pid, config.signal, config.foreground)?;

    /* if --kill-after, give it a grace period then escalate to SIGKILL */
    if let Some(kill_after) = config.kill_after {
        /* no heartbeat, stdin timeout, or throttle during grace period.
         * throttle disabled because re-SIGSTOP would prevent signal handler. */
        let grace_result = wait_with_kqueue(
            child,
            pid,
            Some(kill_after),
            config.confine,
            None,
            None,
            None, /* throttle disabled - process needs to run signal handler */
            None, /* no memory limit during grace period */
        )?;

        match grace_result {
            WaitResult::Exited(status, rusage) => {
                /* mark process exited to prevent PID recycling issues */
                if let Some(ref mut ctx) = throttle_ctx {
                    ctx.state.mark_process_exited();
                }
                return Ok(RunResult::TimedOut {
                    signal: config.signal,
                    killed: false,
                    status: Some(status),
                    rusage: Some(rusage),
                    hook: hook_result,
                    reason: timeout_reason,
                });
            }
            WaitResult::ReceivedSignal(sig) => {
                /* Forward signal during grace period */
                if config.verbose && !config.quiet {
                    crate::eprintln!("timeout: forwarding signal {} to command", signal_name(sig));
                }
                /* resume if throttle had it stopped - prevents deadlock */
                if let Some(ref mut ctx) = throttle_ctx {
                    ctx.state.resume();
                }
                send_signal(pid, sig, config.foreground)?;
                /* wait for child - extract rusage even if wait returns error (child exited) */
                let (status, rusage) = match child.wait() {
                    Ok((s, r)) => (Some(s), Some(r)),
                    Err(_) => (None, None), /* child already reaped or wait failed */
                };
                /* mark process exited to prevent PID recycling issues */
                if let Some(ref mut ctx) = throttle_ctx {
                    ctx.state.mark_process_exited();
                }
                return Ok(RunResult::SignalForwarded {
                    signal: sig,
                    status,
                    rusage,
                });
            }
            WaitResult::TimedOut(_) | WaitResult::MemoryLimitExceeded { .. } => {
                /* Continue to SIGKILL below - shouldn't happen during grace but handle it */
            }
        }

        /* still alive? SIGKILL it */
        if config.verbose && !config.quiet {
            crate::eprintln!("timeout: sending signal SIGKILL to command");
        }

        /* resume if throttle had it stopped - prevents deadlock */
        if let Some(ref mut ctx) = throttle_ctx {
            ctx.state.resume();
        }

        send_signal(pid, Signal::SIGKILL, config.foreground)?;

        let (status, rusage) = child.wait().map_err(|e| match e {
            SpawnError::Wait(errno) => TimeoutError::SpawnError(errno),
            _ => TimeoutError::Internal("wait failed".to_string()),
        })?;

        /* mark process exited to prevent PID recycling issues */
        if let Some(ref mut ctx) = throttle_ctx {
            ctx.state.mark_process_exited();
        }

        Ok(RunResult::TimedOut {
            signal: config.signal,
            killed: true,
            status: Some(status),
            rusage: Some(rusage),
            hook: hook_result,
            reason: timeout_reason,
        })
    } else {
        /* no kill-after, just wait for it to die */
        let (status, rusage) = child.wait().map_err(|e| match e {
            SpawnError::Wait(errno) => TimeoutError::SpawnError(errno),
            _ => TimeoutError::Internal("wait failed".to_string()),
        })?;

        /* mark process exited to prevent PID recycling issues */
        if let Some(ref mut ctx) = throttle_ctx {
            ctx.state.mark_process_exited();
        }

        Ok(RunResult::TimedOut {
            signal: config.signal,
            killed: false,
            status: Some(status),
            rusage: Some(rusage),
            hook: hook_result,
            reason: timeout_reason,
        })
    }
}

/*
 * What happened when we waited on the process.
 */
enum WaitResult {
    Exited(RawExitStatus, ResourceUsage),
    TimedOut(TimeoutReason), /* what triggered: wall clock or stdin idle */
    MemoryLimitExceeded {
        limit_bytes: u64,
        actual_bytes: u64,
    },
    /// Received a signal that should be forwarded to the child
    ReceivedSignal(Signal),
}

/* stdin timeout config for wait_with_kqueue */
struct StdinTimeoutConfig {
    timeout_ns: u64,       /* stdin idle timeout in nanoseconds */
    last_activity_ns: u64, /* timestamp of last stdin activity */
    mode: StdinMode,       /* consume vs passthrough */
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StdinMode {
    Consume,
    Passthrough,
}

/* heartbeat config for wait_with_kqueue */
struct HeartbeatConfig {
    interval_ns: u64, /* heartbeat interval in nanoseconds, 0 = disabled */
    quiet: bool,      /* suppress output */
    pid: i32,         /* child pid for message */
    start_ns: u64,    /* when we started, for elapsed calculation */
}

/* print heartbeat status message to stderr */
/* format elapsed time as "Xm Ys" or "Xs" - integer math only, no floats */
fn print_heartbeat(elapsed_ns: u64, pid: i32) {
    let elapsed_secs = elapsed_ns / 1_000_000_000;
    let mins = elapsed_secs / 60;
    let secs = elapsed_secs % 60;

    if mins > 0 {
        crate::eprintln!(
            "timeout: heartbeat: {}m {}s elapsed, command still running (pid {})",
            mins,
            secs,
            pid
        );
    } else {
        crate::eprintln!(
            "timeout: heartbeat: {}s elapsed, command still running (pid {})",
            secs,
            pid
        );
    }
}

/* get errno - on macOS this is a thread-local via __error() */
#[inline]
fn errno() -> i32 {
    unsafe extern "C" {
        fn __error() -> *mut i32;
    }
    // SAFETY: __error always returns valid pointer on macOS. The dereference and
    // function call share the same invariant (pointer validity for thread-local errno).
    #[allow(clippy::multiple_unsafe_ops_per_block)]
    unsafe {
        *__error()
    }
}

/* stdin poll result - distinguishes readable from idle from closed */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdinPollResult {
    Readable, /* data available */
    Idle,     /* no data, still open */
    Eof,      /* pipe closed / hangup */
}

/* check stdin status without consuming data */
#[inline]
fn stdin_poll_status() -> StdinPollResult {
    let mut pfd = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };

    // SAFETY: poll with nfds=1 and timeout=0 is safe for any fd value.
    let ret = unsafe { libc::poll(&raw mut pfd, 1, 0) };

    if ret <= 0 {
        return StdinPollResult::Idle;
    }

    /* POLLHUP = writer closed the pipe, no more data coming */
    if (pfd.revents & libc::POLLHUP) != 0 && (pfd.revents & libc::POLLIN) == 0 {
        return StdinPollResult::Eof;
    }

    if (pfd.revents & libc::POLLIN) != 0 {
        return StdinPollResult::Readable;
    }

    StdinPollResult::Idle
}

/*
 * wait using kqueue - EVFILT_PROC for exit, EVFILT_TIMER with NOTE_NSECONDS
 * for nanosecond precision, and optionally EVFILT_READ for signal pipe.
 * Direct libc because nix kqueue API keeps changing.
 *
 * With heartbeat: wakes at min(remaining_timeout, next_heartbeat), prints
 * status message on heartbeat intervals, continues waiting until timeout.
 *
 * With stdin timeout: adds EVFILT_READ on fd 0, resets timer on activity,
 * triggers if stdin is idle for the specified duration.
 */
#[allow(clippy::too_many_arguments)]
fn wait_with_kqueue(
    child: &mut RawChild,
    pid: i32,
    wall_timeout: Option<Duration>,
    confine: Confine,
    heartbeat: Option<HeartbeatConfig>,
    mut stdin_timeout: Option<StdinTimeoutConfig>,
    mut throttle: Option<&mut ThrottleContext>,
    memory_limit: Option<MemoryLimitConfig>,
) -> Result<WaitResult> {
    let start_ns = precise_now_ns(confine)?;
    /* None = no wall-clock deadline (run until exit / periodic checks only).
     * Some(ZERO) is a finite, already-reached deadline (immediate). */
    let timeout_ns = wall_timeout.map_or(0, duration_to_ns);

    /* throttle tracking */
    let throttle_interval_ns = throttle
        .as_ref()
        .map(|t| t.cfg.interval_ns)
        .unwrap_or(u64::MAX);
    let mut next_throttle_ns = if throttle_interval_ns < u64::MAX {
        throttle
            .as_ref()
            .map(|t| advance_ns(t.state.last_wall_ns, throttle_interval_ns))
            .unwrap_or(u64::MAX)
    } else {
        u64::MAX
    };

    /* memory limit tracking */
    let memory_check_interval_ns = memory_limit
        .as_ref()
        .map_or(u64::MAX, |m| m.check_interval_ns);
    let mut next_memory_check_ns = if memory_check_interval_ns < u64::MAX {
        advance_ns(start_ns, memory_check_interval_ns)
    } else {
        u64::MAX
    };

    /* heartbeat tracking: next heartbeat fires at start_ns + interval, then every interval */
    let heartbeat_interval_ns = heartbeat.as_ref().map_or(0, |h| h.interval_ns);
    let mut next_heartbeat_ns = if heartbeat_interval_ns > 0 {
        advance_ns(start_ns, heartbeat_interval_ns)
    } else {
        u64::MAX /* disabled */
    };

    /* Get signal pipe fd if available */
    let signal_fd = {
        let fd = SIGNAL_PIPE.load(Ordering::SeqCst);
        if fd >= 0 { Some(fd) } else { None }
    };

    /* stdin timeout tracking */
    /* validate stdin fd before enabling monitoring - fstat returns -1 if fd is invalid */
    let stdin_valid = if stdin_timeout.is_some() {
        // SAFETY: zeroed stat struct is valid for fstat call below
        let mut stat: libc::stat = unsafe { core::mem::zeroed() };
        // SAFETY: fstat with valid stat buffer, fd 0 may or may not be valid
        let result = unsafe { libc::fstat(0, &raw mut stat) };
        if result < 0 {
            /* stdin fd is invalid (closed, bad fd) - disable stdin timeout */
            stdin_timeout = None;
            false
        } else {
            true
        }
    } else {
        false
    };
    let stdin_timeout_ns = stdin_timeout.as_ref().map_or(0, |s| s.timeout_ns);
    /* mutable: updated when stdin EOF/error disables monitoring.
     * only consume mode registers stdin with kqueue - passthrough uses timer-based poll. */
    let mut stdin_enabled = stdin_valid
        && stdin_timeout
            .as_ref()
            .map(|s| matches!(s.mode, StdinMode::Consume))
            .unwrap_or(false);

    /* initial level check for passthrough mode to set activity timestamp */
    if let Some(ref mut stdin_cfg) = stdin_timeout
        && stdin_cfg.mode == StdinMode::Passthrough
        && stdin_valid
    {
        match stdin_poll_status() {
            StdinPollResult::Readable => {
                stdin_cfg.last_activity_ns = precise_now_ns(confine)?;
            }
            StdinPollResult::Eof => {
                /* stdin already closed - disable monitoring */
                stdin_timeout = None;
            }
            StdinPollResult::Idle => { /* nothing to do */ }
        }
    }

    /* create kqueue fd */
    // SAFETY: kqueue() has no preconditions, returns -1 on error (checked below).
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(TimeoutError::Internal(format!(
            "kqueue failed: errno {}",
            errno()
        )));
    }

    /*
     * kqueue filters:
     * - EVFILT_PROC + NOTE_EXIT: wake when process dies, no polling needed
     * - EVFILT_TIMER + NOTE_NSECONDS: nanosecond timer (kernel scheduler adds
     *   ~15-30ms latency anyway, but we're not the bottleneck)
     * - EVFILT_READ on signal pipe: self-pipe trick for forwarding signals
     * - EVFILT_READ on stdin (fd 0): watch for stdin activity
     *
     * EV_ONESHOT on proc/timer means auto-delete after firing.
     * Signal pipe and stdin stay registered for multiple events.
     */
    /* Use fixed-size array instead of Vec to avoid heap allocation */
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
    let mut changes = [
        /* Watch for process exit */
        libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: core::ptr::null_mut(),
        },
        /* High-precision timer - data is nanoseconds */
        libc::kevent {
            ident: 1, /* Timer identifier (arbitrary, just needs to be unique) */
            filter: libc::EVFILT_TIMER,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_NSECONDS,
            /* Safe: duration_to_ns clamps to isize::MAX */
            data: timeout_ns as isize,
            udata: core::ptr::null_mut(),
        },
        /* Signal pipe watcher (may be unused if no signal fd) */
        libc::kevent {
            ident: signal_fd.unwrap_or(0) as usize,
            filter: libc::EVFILT_READ,
            flags: if signal_fd.is_some() { libc::EV_ADD } else { 0 },
            fflags: 0,
            data: 0,
            udata: core::ptr::null_mut(),
        },
        /* Stdin watcher - consume mode only, passthrough uses timer-based poll */
        libc::kevent {
            ident: 0, /* stdin is fd 0 */
            filter: libc::EVFILT_READ,
            flags: if stdin_enabled { libc::EV_ADD } else { 0 },
            fflags: 0,
            data: 0,
            udata: core::ptr::null_mut(),
        },
    ];
    /*
     * count active changes dynamically:
     * - proc and timer are always active (indices 0, 1)
     * - signal pipe is active if signal_fd is set (index 2)
     * - stdin is active if enabled or being deleted (index 3)
     *
     * Note: kevent entries with flags=0 are no-ops, but we only include
     * entries up to num_changes to avoid any potential issues.
     */

    /* Buffer for returned events - we only need one */
    let mut event = libc::kevent {
        ident: 0,
        filter: 0,
        flags: 0,
        fflags: 0,
        data: 0,
        udata: core::ptr::null_mut(),
    };

    /*
     * kevent() atomically registers filters and waits. No race condition.
     *
     * EINTR: signal interrupted us, recalculate remaining time and retry.
     * ESRCH: process already dead, just reap it.
     *
     * No timeout arg to kevent, the timer filter handles it.
     * With heartbeat: timer fires at min(remaining_timeout, time_to_next_heartbeat).
     * With stdin timeout: timer fires at min(remaining_timeout, stdin_deadline).
     */
    /* no wall-clock deadline: u64::MAX is never reached, so the loop wakes only on
     * periodic checks (mem/throttle/heartbeat/stdin) and EVFILT_PROC. */
    let deadline_ns = match wall_timeout {
        Some(_) => advance_ns(start_ns, timeout_ns),
        None => u64::MAX,
    };

    loop {
        /* check if we've passed deadline */
        let now_ns = precise_now_ns(confine)?;
        if deadline_reached(now_ns, deadline_ns) {
            // SAFETY: kq is a valid fd, close is always safe
            unsafe { libc::close(kq) };
            return Ok(WaitResult::TimedOut(TimeoutReason::WallClock));
        }
        let remaining_timeout_ns = remaining_ns(now_ns, deadline_ns);

        /* check stdin idle timeout using checked arithmetic for invariant detection */
        if let Some(ref stdin_cfg) = stdin_timeout {
            /* elapsed_ns returns None if now < last_activity (clock anomaly) */
            match elapsed_ns(stdin_cfg.last_activity_ns, now_ns) {
                Some(idle_ns) if idle_ns >= stdin_timeout_ns => {
                    // SAFETY: kq is a valid fd
                    unsafe { libc::close(kq) };
                    return Ok(WaitResult::TimedOut(TimeoutReason::StdinIdle));
                }
                None => {
                    /* clock went backwards - log and continue with 0 idle time */
                    /* this shouldn't happen but better than silent misbehavior */
                }
                Some(_) => { /* still within timeout */ }
            }
        }

        /* calculate next wake time: min(remaining timeout, time to next heartbeat, stdin timeout, memory check) */
        let time_to_heartbeat = remaining_ns(now_ns, next_heartbeat_ns);
        let time_to_stdin_deadline = if let Some(ref stdin_cfg) = stdin_timeout {
            /* use checked helper - returns remaining time or 0 if already exceeded */
            time_to_idle_timeout(stdin_cfg.last_activity_ns, now_ns, stdin_timeout_ns).unwrap_or(0) /* clock anomaly: treat as timed out */
        } else {
            u64::MAX
        };
        let time_to_throttle = remaining_ns(now_ns, next_throttle_ns);
        let time_to_memory_check = remaining_ns(now_ns, next_memory_check_ns);
        let next_wake_ns = remaining_timeout_ns
            .min(time_to_heartbeat)
            .min(time_to_stdin_deadline)
            .min(time_to_throttle)
            .min(time_to_memory_check);

        /* update timer to next wake time */
        #[allow(clippy::cast_possible_wrap)]
        {
            changes[1].data = next_wake_ns.min(MAX_TIMER_NS) as isize;
        }

        /* calculate how many changes to submit:
         * - indices 0,1 (proc+timer) always active
         * - index 2 (signal pipe) if present
         * - index 3 (stdin) if enabled OR being deleted
         */
        let stdin_active = stdin_enabled || changes[3].flags == libc::EV_DELETE;
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        let num_changes: i32 = 2 + i32::from(signal_fd.is_some()) + i32::from(stdin_active);

        // SAFETY: kq is a valid kqueue fd. changes is a valid slice of kevent structs.
        // event is a valid buffer for one kevent. Timeout is null (wait forever).
        // kevent() is the standard BSD API for event notification.
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        let n = unsafe {
            libc::kevent(
                kq,
                changes.as_ptr(),
                num_changes,
                &raw mut event,
                1,
                core::ptr::null(), /* No timeout - timer event handles it */
            )
        };

        /* after kevent returns, clear EV_DELETE to avoid re-submitting */
        if changes[3].flags == libc::EV_DELETE {
            changes[3].flags = 0;
        }

        if n < 0 {
            let err = errno();
            /* EINTR: signal interrupted us, retry with remaining time */
            if err == libc::EINTR {
                continue;
            }
            /* ESRCH: process already gone, reap it */
            if err == libc::ESRCH {
                // SAFETY: kq is a valid fd
                unsafe { libc::close(kq) };
                /* try non-blocking first, fall back to blocking wait */
                let Some((status, rusage)) = child.try_wait().ok().flatten() else {
                    /* ESRCH from kernel but child not reaped yet - use blocking wait.
                     * ECHILD error means already reaped elsewhere - treat as timeout. */
                    let wait_result = child.wait();
                    return match wait_result {
                        Ok((status, rusage)) => Ok(WaitResult::Exited(status, rusage)),
                        Err(SpawnError::Wait(e)) if e == libc::ECHILD => {
                            Ok(WaitResult::TimedOut(TimeoutReason::WallClock))
                        }
                        Err(e) => Err(TimeoutError::Internal(format!("wait failed: {}", e))),
                    };
                };

                return Ok(WaitResult::Exited(status, rusage));
            }
            // SAFETY: kq is a valid fd
            unsafe { libc::close(kq) };
            return Err(TimeoutError::Internal(format!(
                "kevent failed: errno {err}"
            )));
        }

        /* handle stdin activity - reset the idle timer */
        if event.filter == libc::EVFILT_READ && event.ident == 0 && stdin_enabled {
            /* EV_EOF means stdin is gone - disable monitoring */
            if (event.flags & libc::EV_EOF) != 0 {
                stdin_timeout = None;
                stdin_enabled = false;
                changes[3].flags = libc::EV_DELETE;
                changes[0].flags = libc::EV_ADD | libc::EV_ONESHOT;
                continue;
            }

            /* consume mode only - passthrough never registers stdin with kqueue.
             * drain any available data to prevent busy-loop - we just care about activity */
            let mut buf = [0u8; 1024];
            // SAFETY: read from stdin (fd 0) with valid buffer
            let bytes_read = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };

            if bytes_read == 0 {
                /* EOF on stdin - no more input possible, disable stdin monitoring.
                 * This prevents busy-loop when stdin is /dev/null or closed pipe. */
                stdin_timeout = None;
                stdin_enabled = false;
                /* remove stdin filter from kqueue, then clear flags to avoid re-submitting */
                changes[3].flags = libc::EV_DELETE;
            } else if bytes_read > 0 {
                /* got actual data - reset the idle timer */
                let now_ns = precise_now_ns(confine)?;
                if let Some(ref mut stdin_cfg) = stdin_timeout {
                    stdin_cfg.last_activity_ns = now_ns;
                }
            }
            /* bytes_read < 0: EAGAIN/EWOULDBLOCK or error - just continue */

            /* re-register proc watcher (oneshot) */
            changes[0].flags = libc::EV_ADD | libc::EV_ONESHOT;
            continue;
        }

        /* got an event - check if it's a heartbeat tick or something else */
        if event.filter == libc::EVFILT_TIMER {
            let now_ns = precise_now_ns(confine)?;

            /* throttle check */
            if let Some(ref mut throttle_ctx) = throttle
                && deadline_reached(now_ns, next_throttle_ns)
            {
                throttle_ctx.state.update(&throttle_ctx.cfg, now_ns)?;
                next_throttle_ns = advance_ns(now_ns, throttle_interval_ns);
            }

            /* memory limit check */
            if let Some(ref mem_cfg) = memory_limit
                && deadline_reached(now_ns, next_memory_check_ns)
            {
                if let Some(current_bytes) = crate::proc_info::get_process_memory(pid)
                    && current_bytes > mem_cfg.limit_bytes
                {
                    // SAFETY: kq is a valid fd
                    unsafe { libc::close(kq) };
                    return Ok(WaitResult::MemoryLimitExceeded {
                        limit_bytes: mem_cfg.limit_bytes,
                        actual_bytes: current_bytes,
                    });
                }
                next_memory_check_ns = advance_ns(now_ns, memory_check_interval_ns);
            }

            /* passthrough mode: level check without consuming data */
            if let Some(ref mut stdin_cfg) = stdin_timeout
                && stdin_cfg.mode == StdinMode::Passthrough
            {
                match stdin_poll_status() {
                    StdinPollResult::Readable => {
                        stdin_cfg.last_activity_ns = now_ns;
                    }
                    StdinPollResult::Eof => {
                        /* stdin closed - disable monitoring to prevent false idle timeout */
                        stdin_timeout = None;
                    }
                    StdinPollResult::Idle => { /* nothing to do */ }
                }
            }

            /* check stdin timeout first using checked arithmetic */
            if let Some(ref stdin_cfg) = stdin_timeout {
                match elapsed_ns(stdin_cfg.last_activity_ns, now_ns) {
                    Some(idle_ns) if idle_ns >= stdin_timeout_ns => {
                        // SAFETY: kq is a valid fd
                        unsafe { libc::close(kq) };
                        return Ok(WaitResult::TimedOut(TimeoutReason::StdinIdle));
                    }
                    _ => { /* within timeout or clock anomaly - continue */ }
                }
            }

            /* heartbeat tick: we haven't reached deadline yet, timer fired for heartbeat */
            if heartbeat_interval_ns > 0
                && !deadline_reached(now_ns, deadline_ns)
                && deadline_reached(now_ns, next_heartbeat_ns)
            {
                /* print heartbeat message */
                if let Some(ref hb) = heartbeat
                    && !hb.quiet
                {
                    /* elapsed_ns validated: hb.start_ns <= now_ns (start before now) */
                    let elapsed = elapsed_ns(hb.start_ns, now_ns).unwrap_or(0);
                    print_heartbeat(elapsed, hb.pid);
                }
                /* schedule next heartbeat */
                next_heartbeat_ns = advance_ns(now_ns, heartbeat_interval_ns);
                /* re-register the timer for next wake (proc watcher is oneshot, re-add) */
                changes[0].flags = libc::EV_ADD | libc::EV_ONESHOT;
                continue;
            }

            /* wall clock deadline reached */
            if deadline_reached(now_ns, deadline_ns) {
                // SAFETY: kq is a valid fd
                unsafe { libc::close(kq) };
                return Ok(WaitResult::TimedOut(TimeoutReason::WallClock));
            }

            /* timer fired for stdin timeout check - continue loop to recalculate */
            changes[0].flags = libc::EV_ADD | libc::EV_ONESHOT;
            continue;
        }

        /* check for registration errors inside loop */
        if (event.flags & libc::EV_ERROR) != 0 {
            #[allow(clippy::cast_possible_truncation)]
            let err_code = event.data as i32;

            /* EBADF on stdin (ident 0) - stdin became invalid, disable monitoring */
            if err_code == libc::EBADF && event.ident == 0 {
                stdin_timeout = None;
                stdin_enabled = false;
                /* remove stdin filter from kqueue */
                changes[3].flags = libc::EV_DELETE;
                changes[0].flags = libc::EV_ADD | libc::EV_ONESHOT;
                continue;
            }

            /* ENOENT on stdin (ident 0) - filter doesn't exist (already deleted), ignore */
            if err_code == libc::ENOENT && event.ident == 0 {
                /* clear EV_DELETE flag since filter is gone */
                changes[3].flags = 0;
                changes[0].flags = libc::EV_ADD | libc::EV_ONESHOT;
                continue;
            }

            /* ESRCH = process gone - break to handle below */
            if err_code == libc::ESRCH {
                break;
            }

            /* other errors are fatal */
            // SAFETY: kq is a valid fd
            unsafe { libc::close(kq) };
            return Err(TimeoutError::Internal(format!(
                "kqueue event registration failed: errno {}",
                err_code
            )));
        }

        /* got a non-heartbeat/non-stdin result, exit loop */
        break;
    }

    /* handle ESRCH after loop exit */
    if (event.flags & libc::EV_ERROR) != 0 {
        #[allow(clippy::cast_possible_truncation)]
        let err_code = event.data as i32;
        /* ESRCH = process gone, that's fine */
        if err_code == libc::ESRCH {
            // SAFETY: kq is a valid fd
            unsafe { libc::close(kq) };
            /* try non-blocking first, fall back to blocking wait */
            match child.try_wait() {
                Ok(Some((status, rusage))) => return Ok(WaitResult::Exited(status, rusage)),
                Ok(None) | Err(_) => match child.wait() {
                    Ok((status, rusage)) => return Ok(WaitResult::Exited(status, rusage)),
                    Err(SpawnError::Wait(e)) if e == libc::ECHILD => {
                        return Ok(WaitResult::TimedOut(TimeoutReason::WallClock));
                    }
                    Err(e) => {
                        return Err(TimeoutError::Internal(format!("wait failed: {}", e)));
                    }
                },
            }
        }
    }

    // SAFETY: kq is a valid fd
    unsafe { libc::close(kq) };

    /* EVFILT_PROC = exited, EVFILT_TIMER = timed out, EVFILT_READ = signal received */
    if event.filter == libc::EVFILT_PROC {
        let (status, rusage) = child.wait().map_err(|e| match e {
            SpawnError::Wait(errno) => TimeoutError::SpawnError(errno),
            _ => TimeoutError::Internal("wait failed".to_string()),
        })?;
        return Ok(WaitResult::Exited(status, rusage));
    }

    if event.filter == libc::EVFILT_READ {
        /* signal pipe became readable - a signal was received */
        let Some(fd) = signal_fd else {
            /* spurious wakeup - treat as timeout */
            return Ok(WaitResult::TimedOut(TimeoutReason::WallClock));
        };
        let Some(sig) = read_signal_from_pipe(fd) else {
            /* pipe readable but no signal byte yet - treat as timeout */
            return Ok(WaitResult::TimedOut(TimeoutReason::WallClock));
        };
        return Ok(WaitResult::ReceivedSignal(sig));
    }

    Ok(WaitResult::TimedOut(TimeoutReason::WallClock))
}

/*
 * Run the on-timeout hook command with PID substitution.
 * The hook has a time limit to prevent hanging. We log but don't fail
 * if the hook fails - the main timeout behavior must proceed.
 *
 * Substitution: %p -> PID, %% -> literal %
 *
 * Note: If the hook spawns processes that create their own process groups
 * (e.g., via setsid or nohup), those won't be killed when the hook times out.
 * Such orphans get reparented to init. For safety-critical use, hooks should
 * not spawn long-lived background processes.
 */
fn run_on_timeout_hook(cmd: &str, pid: i32, config: &RunConfig) -> HookResult {
    /* use 0 as fallback for timing if timebase fails - hook timing is best-effort */
    let start_ns = precise_now_ns(config.confine).unwrap_or(0);

    /* Expand %p to PID, %% to literal % */
    let expanded_cmd = cmd
        .replace("%%", "\x00PERCENT\x00") /* placeholder for %% */
        .replace("%p", &format!("{}", pid))
        .replace("\x00PERCENT\x00", "%"); /* restore literal % */

    if config.verbose && !config.quiet {
        crate::eprintln!("timeout: running on-timeout hook: {}", expanded_cmd);
    }

    /* Run via shell to support complex commands.
     * Use process group so we can kill hook and all its children on timeout. */
    let spawn_result = spawn_command("sh", &[String::from("-c"), expanded_cmd], true);

    let mut child = match spawn_result {
        Ok(c) => c,
        Err(e) => {
            if config.verbose && !config.quiet {
                crate::eprintln!("timeout: on-timeout hook failed to start: {}", e);
            }
            return HookResult {
                ran: false,
                exit_code: None,
                timed_out: false,
                /* use checked elapsed - fallback to 0 on clock anomaly */
                elapsed_ms: elapsed_ns(start_ns, precise_now_ns(config.confine).unwrap_or(0))
                    .unwrap_or(0)
                    / 1_000_000,
            };
        }
    };

    /* Wait using kqueue for zero-CPU waiting */
    let hook_wait_result =
        wait_for_hook_with_kqueue(&mut child, config.on_timeout_limit, config.confine);
    /* use checked elapsed - fallback to 0 on clock anomaly */
    let elapsed_ms =
        elapsed_ns(start_ns, precise_now_ns(config.confine).unwrap_or(0)).unwrap_or(0) / 1_000_000;

    match hook_wait_result {
        HookWaitResult::Exited(status) => {
            let exit_code = status.code();
            if config.verbose
                && !config.quiet
                && let Some(code) = exit_code
                && code != 0
            {
                crate::eprintln!("timeout: on-timeout hook exited with code {}", code);
            }
            HookResult {
                ran: true,
                exit_code,
                timed_out: false,
                elapsed_ms,
            }
        }
        HookWaitResult::TimedOut => {
            if config.verbose && !config.quiet {
                crate::eprintln!("timeout: on-timeout hook timed out, killing");
            }
            /* Kill entire process group to get grandchildren too */
            let pid = child.id() as i32;
            // SAFETY: killpg with valid pid and signal is safe
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            let _ = child.wait();
            HookResult {
                ran: true,
                exit_code: None,
                timed_out: true,
                elapsed_ms,
            }
        }
        HookWaitResult::Error(e) => {
            if config.verbose && !config.quiet {
                crate::eprintln!("timeout: on-timeout hook wait failed: {}", e);
            }
            HookResult {
                ran: true,
                exit_code: None,
                timed_out: false,
                elapsed_ms,
            }
        }
    }
}

/* Result of waiting for hook process */
enum HookWaitResult {
    Exited(RawExitStatus),
    TimedOut,
    Error(String),
}

/*
 * Wait for hook process using kqueue - zero CPU while waiting.
 * Simpler than wait_with_kqueue since we don't need signal forwarding.
 */
fn wait_for_hook_with_kqueue(
    child: &mut RawChild,
    timeout: Duration,
    confine: Confine,
) -> HookWaitResult {
    #[allow(clippy::cast_possible_wrap)]
    let pid = child.id() as i32;
    /* use 0 as fallback for timing if timebase fails - hook timing is best-effort */
    let start_ns = precise_now_ns(confine).unwrap_or(0);
    let timeout_ns = duration_to_ns(timeout);
    let deadline_ns = advance_ns(start_ns, timeout_ns);

    /* create kqueue fd */
    // SAFETY: kqueue() has no preconditions, returns -1 on error (checked below).
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return HookWaitResult::Error(format!("kqueue failed: errno {}", errno()));
    }

    /* Watch for process exit and set timer */
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
    let mut changes = [
        libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: core::ptr::null_mut(),
        },
        libc::kevent {
            ident: 2, /* different from main timer */
            filter: libc::EVFILT_TIMER,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_NSECONDS,
            data: timeout_ns as isize,
            udata: core::ptr::null_mut(),
        },
    ];

    let mut event = libc::kevent {
        ident: 0,
        filter: 0,
        flags: 0,
        fflags: 0,
        data: 0,
        udata: core::ptr::null_mut(),
    };

    loop {
        /* Recalculate remaining time (handles EINTR correctly) */
        let now_ns = precise_now_ns(confine).unwrap_or(deadline_ns); /* on error, trigger timeout */
        if deadline_reached(now_ns, deadline_ns) {
            // SAFETY: kq is a valid fd from kqueue() above.
            unsafe { libc::close(kq) };
            return HookWaitResult::TimedOut;
        }
        let remaining_timeout_ns = remaining_ns(now_ns, deadline_ns);
        changes[1].data = remaining_timeout_ns.min(MAX_TIMER_NS) as isize;

        // SAFETY: kq is a valid kqueue fd. changes is a valid slice of kevent structs.
        // event is a valid buffer for one kevent. Timeout is null (wait forever).
        #[allow(clippy::cast_possible_wrap)]
        let n = unsafe {
            libc::kevent(
                kq,
                changes.as_ptr(),
                changes.len() as i32,
                &raw mut event,
                1,
                core::ptr::null(),
            )
        };

        if n < 0 {
            let err = errno();
            if err == libc::EINTR {
                continue;
            }
            if err == libc::ESRCH {
                /* Process already gone */
                // SAFETY: kq is a valid fd from kqueue() above.
                unsafe { libc::close(kq) };
                return match child.try_wait() {
                    Ok(Some((status, _rusage))) => HookWaitResult::Exited(status),
                    Ok(None) => match child.wait() {
                        Ok((status, _rusage)) => HookWaitResult::Exited(status),
                        Err(e) => HookWaitResult::Error(format!("{}", e)),
                    },
                    Err(e) => HookWaitResult::Error(format!("{}", e)),
                };
            }
            // SAFETY: kq is a valid fd from kqueue() above.
            unsafe { libc::close(kq) };
            return HookWaitResult::Error(format!("kevent failed: errno {}", err));
        }
        break;
    }

    /* Check for registration errors */
    if (event.flags & libc::EV_ERROR) != 0 {
        #[allow(clippy::cast_possible_truncation)]
        let err_code = event.data as i32;
        // SAFETY: kq is a valid fd from kqueue() above.
        unsafe { libc::close(kq) };
        if err_code == libc::ESRCH {
            return match child.wait() {
                Ok((status, _rusage)) => HookWaitResult::Exited(status),
                Err(e) => HookWaitResult::Error(format!("{}", e)),
            };
        }
        return HookWaitResult::Error(format!("kqueue registration failed: errno {}", err_code));
    }

    // SAFETY: kq is a valid fd from kqueue() above.
    unsafe { libc::close(kq) };

    if event.filter == libc::EVFILT_PROC {
        match child.wait() {
            Ok((status, _rusage)) => HookWaitResult::Exited(status),
            Err(e) => HookWaitResult::Error(format!("{}", e)),
        }
    } else {
        HookWaitResult::TimedOut
    }
}

/*
 * Send signal to child.
 *
 * Normal mode: killpg() signals the whole process group, catches shell
 * scripts with children. ESRCH means it's already dead, that's fine.
 *
 * Foreground mode: just signal the one process. For interactive stuff
 * that needs TTY. Grandchildren won't get the signal though.
 *
 * killpg can fail with ESRCH even when process exists (race conditions),
 * so we fall back to regular kill().
 */
fn send_signal(pid: i32, signal: Signal, foreground: bool) -> Result<()> {
    let sig = signal.as_raw();

    if foreground {
        // SAFETY: kill() is safe with any pid/signal combo, returns -1 on error
        let ret = unsafe { libc::kill(pid, sig) };
        if ret == 0 {
            return Ok(());
        }
        let err = errno();
        if err == libc::ESRCH {
            return Ok(()); // already dead, that's fine
        }
        return Err(TimeoutError::SignalError(err));
    }

    /*
     * try process group first. if ESRCH, fall back to just the process.
     * orphaned children get reparented to init - that's unix for you.
     */
    // SAFETY: killpg() is safe with any pid/signal combo, returns -1 on error
    let ret = unsafe { libc::killpg(pid, sig) };
    if ret == 0 {
        return Ok(());
    }

    let err = errno();
    if err == libc::ESRCH {
        /* group gone, try process directly */
        // SAFETY: kill() is safe with any pid/signal combo, returns -1 on error
        let ret = unsafe { libc::kill(pid, sig) };
        if ret == 0 {
            return Ok(());
        }
        let err = errno();
        if err == libc::ESRCH {
            return Ok(()); // already dead
        }
        return Err(TimeoutError::SignalError(err));
    }

    Err(TimeoutError::SignalError(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_result_exit_code_timeout() {
        let result = RunResult::TimedOut {
            signal: Signal::SIGTERM,
            killed: false,
            status: None,
            rusage: None,
            hook: None,
            reason: TimeoutReason::WallClock,
        };

        assert_eq!(result.exit_code(false, 124), 124);
        assert_eq!(result.exit_code(true, 124), 143); /* 128 + 15 */
    }

    #[test]
    fn test_run_result_exit_code_killed() {
        let result = RunResult::TimedOut {
            signal: Signal::SIGTERM,
            killed: true,
            status: None,
            rusage: None,
            hook: None,
            reason: TimeoutReason::WallClock,
        };

        assert_eq!(result.exit_code(false, 124), 124);
        assert_eq!(result.exit_code(true, 124), 137); /* 128 + 9 */
    }

    #[test]
    fn test_custom_timeout_exit_code() {
        let result = RunResult::TimedOut {
            signal: Signal::SIGTERM,
            killed: false,
            status: None,
            rusage: None,
            hook: None,
            reason: TimeoutReason::WallClock,
        };

        assert_eq!(result.exit_code(false, 42), 42);
        assert_eq!(result.exit_code(false, 0), 0);
    }

    /* Skip under Miri: libc::kill is an unsupported foreign function */
    #[test]
    #[cfg(not(miri))]
    fn test_send_signal_to_nonexistent_process() {
        /* ESRCH should be handled gracefully */
        let fake_pid = 99999i32;
        let result = send_signal(fake_pid, Signal::SIGTERM, true);
        assert!(result.is_ok(), "ESRCH should be handled gracefully");
    }

    /* test that timebase validation works on the happy path (denom != 0) */
    #[test]
    #[cfg_attr(miri, ignore)] /* mach_timebase_info is FFI */
    fn test_get_timebase_info_succeeds() {
        let result = get_timebase_info();
        assert!(result.is_ok(), "timebase info should succeed on macOS");
        let (numer, denom) = result.unwrap();
        assert!(numer > 0, "numer should be positive");
        assert!(denom > 0, "denom should be positive (validated)");
    }

    /* test that wall_now_ns returns a reasonable value */
    #[test]
    #[cfg_attr(miri, ignore)] /* mach_continuous_time is FFI */
    fn test_wall_now_ns_succeeds() {
        let result = wall_now_ns();
        assert!(result.is_ok(), "wall_now_ns should succeed on macOS");
        let ns = result.unwrap();
        assert!(ns > 0, "time should be positive");
    }

    /* test that precise_now_ns works in both modes */
    #[test]
    #[cfg_attr(miri, ignore)] /* uses FFI */
    fn test_precise_now_ns_both_modes() {
        use crate::args::Confine;

        let wall = precise_now_ns(Confine::Wall);
        assert!(wall.is_ok(), "wall mode should succeed");

        let active = precise_now_ns(Confine::Active);
        assert!(active.is_ok(), "active mode should succeed");
    }
}

# Resource Limits

`procguard` provides three complementary resource limiting mechanisms. Each uses a different enforcement strategy with distinct trade-offs.

## Quick Reference

| Option | Scope | Enforcement | Granularity | macOS Support |
|--------|-------|-------------|-------------|---------------|
| `--mem-limit` | Process | Polling | ~100ms | ✓ Full |
| `--cpu-time` | Process | Kernel (RLIMIT_CPU) | 1 second | ✓ Full |
| `--cpu-percent` | Process | Polling + SIGSTOP/SIGCONT | ~100ms | ✓ Full |

## Memory Limit (`--mem-limit`)

Soft memory limit enforced via polling. Terminates process when physical memory footprint exceeds the specified limit.

### Usage

```bash
timeout --mem-limit 512M 1h ./my-program
timeout --mem-limit 2G --kill-after 5s 30m ./memory-hog
```

### Accepted Formats

| Format | Value |
|--------|-------|
| `1024` | 1024 bytes |
| `64K` / `64KB` | 65,536 bytes |
| `512M` / `512MB` | 536,870,912 bytes |
| `2G` / `2GB` | 2,147,483,648 bytes |
| `1T` / `1TB` | 1,099,511,627,776 bytes |

Units are **binary** (1K = 1024), case-insensitive.

### Implementation Details

1. **Polling mechanism**: Checks memory every 100ms via `proc_pid_rusage()`
2. **Metric used**: `ri_phys_footprint` (physical memory, not virtual)
3. **No entitlements required**: Uses public Darwin libproc API
4. **Signal on exceed**: Sends configured signal (default SIGTERM)
5. **Grace period**: Honors `--kill-after` for escalation to SIGKILL

### Why Not RLIMIT_AS?

macOS does **not enforce** `RLIMIT_AS` (address space limit). The `setrlimit()` call returns `EINVAL`. We still attempt to set it (for potential future macOS support), but enforcement relies entirely on polling via `proc_pid_rusage()`.

### Trade-offs

| Pros | Cons |
|------|------|
| Actually works on macOS | ~100ms detection latency |
| Measures real physical memory | Slight CPU overhead from polling |
| No kernel support needed | Only monitors main process (not children) |

---

## CPU Time Limit (`--cpu-time`)

Hard CPU time limit enforced by the kernel via `RLIMIT_CPU`. Process receives SIGXCPU then SIGKILL when it exceeds total CPU seconds.

### Usage

```bash
timeout --cpu-time 30s 1h ./compute-job
timeout --cpu-time 5m 2h ./batch-process
```

### Accepted Formats

Same as timeout duration: `30`, `30s`, `5m`, `1h`, `1.5h`

### Implementation Details

1. **Kernel enforced**: Uses `setrlimit(RLIMIT_CPU, ...)`
2. **Signal sequence**: SIGXCPU at limit, then SIGKILL ~1 second later
3. **Granularity**: 1 second (kernel limitation)
4. **Cumulative**: Counts total CPU time across all cores

Note: Both soft and hard limits are set to the same value, so SIGKILL follows quickly after SIGXCPU.

### Behavior

```
CPU time consumed    Signal
─────────────────    ──────
< limit              (none)
= limit              SIGXCPU
> limit              SIGKILL
```

### Trade-offs

| Pros | Cons |
|------|------|
| Kernel enforced (reliable) | 1-second granularity only |
| Zero polling overhead | Process can't catch SIGKILL |
| Includes all threads | Can't distinguish user vs system time |

---

## CPU Percent Throttle (`--cpu-percent`)

Throttles CPU usage to a percentage by suspending/resuming the process. Uses SIGSTOP/SIGCONT signals with integral control for precise convergence.

### Usage

```bash
# Limit to 50% of one CPU core
timeout --cpu-percent 50 1h ./background-task

# Limit to 2 full cores on multi-core system
timeout --cpu-percent 200 1h ./parallel-job

# Aggressive throttle for truly background work
timeout --cpu-percent 10 1h ./low-priority-task
```

### Multi-Core Semantics

CPU percentage is **total across all cores**:

| Value | Meaning |
|-------|---------|
| 50 | Half of one core |
| 100 | One full core |
| 200 | Two full cores |
| 400 | Four full cores |
| 1400 | All 14 cores (M4 Pro) |

A 4-thread process running at 100% on each core shows 400% CPU. Setting `--cpu-percent 50` will aggressively throttle it.

### Algorithm: Integral Control

The throttle uses **integral control** rather than instantaneous measurement:

```
if total_cpu_time > total_wall_time × target%:
    SIGSTOP (suspend)
else:
    SIGCONT (resume)
```

This creates a "debt" mechanism:
- Process runs hot early → accumulates debt → gets suspended
- Wall clock catches up → debt paid → resumes
- Over time, converges to **exact** target percentage

Previous delta-based approaches compared per-interval CPU usage, which aliased with the scheduler and converged to ~50% regardless of target.

### Signal Behavior

| Signal | Catchable | Notes |
|--------|-----------|-------|
| SIGSTOP | No | Process cannot prevent suspension |
| SIGCONT | Yes | Process may observe these signals |

**Important**: Only the main process is throttled, not child processes.

### Deadlock Prevention

When terminating a throttled process:

1. **Resume first**: Send SIGCONT before any termination signal
2. **Disable during grace**: Throttle disabled during `--kill-after` period
3. **Cleanup on drop**: `Drop` impl sends SIGCONT if still suspended

Without this, sending SIGTERM to a SIGSTOP'd process creates deadlock—the process can't execute its signal handler while suspended.

### Trade-offs

| Pros | Cons |
|------|------|
| Precise long-term convergence | ~100ms response latency |
| Works on macOS | Process sees SIGCONT signals |
| Multi-core aware | Only throttles main process |
| No kernel support needed | Initial oscillation possible |

### Warning for Low Values

Values below 10% trigger a warning:

```
warning: --cpu-percent 5 is very low; may cause stuttery execution
```

Very low percentages cause frequent stop/start cycles that may affect process behavior.

---

## Combining Limits

All three limits can be used together:

```bash
timeout \
  --mem-limit 1G \
  --cpu-time 5m \
  --cpu-percent 50 \
  --kill-after 10s \
  1h ./resource-hungry-job
```

### Enforcement Order

Multiple limits run concurrently. The **first violation** terminates the process:

| Limit | Enforcement | Termination |
|-------|-------------|-------------|
| Memory limit | Polled every 100ms | SIGTERM (+ SIGKILL after grace) |
| CPU time | Kernel async | SIGXCPU then SIGKILL |
| CPU percent | Polled every 100ms | Throttling only (no termination) |
| Wall clock | Timer-based | SIGTERM (+ SIGKILL after grace) |

Note: CPU time (RLIMIT_CPU) is enforced by the kernel independently of the polling loop, so it may trigger between polls.

### Without a Wall-Clock Timeout

All limits above are enforced whether or not a wall-clock timeout is set. Passing a
duration of `0` ("no timeout", run forever) still enforces `--mem-limit`,
`--cpu-time`, `--cpu-percent`, and the `--heartbeat` / `--stdin-timeout` monitors —
the process runs until it exits, a limit is breached, or a signal arrives.

```bash
# no wall-clock kill, but memory is still capped at 512M
timeout --mem-limit 512M 0 ./long-running-service
```

### JSON Output

With `--json`, limits appear in the output:

```json
{
  "schema_version": 8,
  "status": "completed",
  "limits": {
    "mem_bytes": 1073741824,
    "cpu_time_ms": 300000,
    "cpu_percent": 50,
    "cpu_interval_ms": 100,
    "cpu_sleep_ms": 50
  }
}
```

Memory limit violations include additional fields:

```json
{
  "status": "memory_limit",
  "limit_bytes": 1073741824,
  "actual_bytes": 1150000000
}
```

---

## Platform Notes

### macOS-Specific Behavior

| Feature | macOS Behavior |
|---------|----------------|
| RLIMIT_AS | Not enforced (EINVAL) |
| RLIMIT_CPU | Fully supported |
| proc_pid_rusage | Works without entitlements |
| SIGSTOP/SIGCONT | Fully supported |

### Why Polling?

macOS lacks several Linux features:
- No `cgroups` for resource isolation
- No `prctl()` for process control
- `RLIMIT_AS` not enforced

Polling via `proc_pid_rusage()` is the reliable cross-version approach.

---

## Examples

### Background Build Server

Keep builds from consuming all resources:

```bash
timeout --cpu-percent 200 --mem-limit 4G 2h make -j8
```

### Memory-Constrained Testing

Ensure program handles low memory:

```bash
timeout --mem-limit 64M 1m ./memory-test
```

### Fair-Share Batch Processing

Prevent any single job from monopolizing CPU:

```bash
timeout --cpu-time 10m --cpu-percent 50 1h ./batch-job
```

### Strict Resource Box

Maximum constraints for untrusted code:

```bash
timeout \
  --mem-limit 256M \
  --cpu-time 30s \
  --cpu-percent 25 \
  --kill-after 5s \
  1m ./untrusted-script
```

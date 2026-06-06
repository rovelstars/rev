# rev benchmarks

Microbenchmarks comparing **rev/WireBus** against **systemd/DBus** for
equivalent operations. Built to be driven by [hyperfine] so the
statistical framing (warmup, repeated runs, mean ± σ) is handled outside
the bench code.

[hyperfine]: https://github.com/sharkdp/hyperfine

## Design

Each bench is a small CLI binary. It parses an iteration count `N`, opens
one persistent connection (or performs `N` short-lived connections),
does the operation in a tight loop, and exits. We use hyperfine to run
the same binary multiple times for statistical confidence rather than
doing that ourselves.

> **Why not just `hyperfine 'dbus-send …' 'some-wirebus-call …'`?**
> Shell + process startup is ~1–5 ms on a typical system. Most IPC ops
> are 10–100 µs. Timing one-shot commands via hyperfine makes you
> compare `fork+exec+libc-startup` times, not the IPC.
> With N=10 000 internal iterations, the wrapper cost is amortised to
> noise and per-op latency = `hyperfine mean / N`.

## Prerequisites

- `hyperfine` installed (`cargo install hyperfine`, or from distro)
- `rev` built and running. These benchmarks connect to rev's System
  Highway at `$REV_SOCK` (default `./rev.sock`, matching rev's debug
  build). Start rev in another terminal: `cargo run --bin rev` from
  `../../`.
- DBus session bus available (any normal desktop session has one).

Build the benchmarks once, in release mode — LTO + single codegen unit
matter when you're looking at sub-microsecond syscall paths:

```
cd rev/benchmarks
cargo build --release
```

Binaries land at `target/release/bench-*`. The examples below assume
you're running from `rev/benchmarks/`.

## Running

### Round-trip latency (persistent connection)

Each iteration = one request frame + one reply frame. The WireBus bench
calls `ListBus`; the DBus bench calls `org.freedesktop.DBus.Peer.Ping`.
Both are the cheapest "full round-trip" their broker supports.

```
hyperfine --warmup 3 --runs 10 \
    'target/release/bench-wirebus-rtt 10000' \
    'target/release/bench-dbus-rtt 10000'
```

Each binary also prints its own per-op latency to stderr, which is
useful as a cross-check against hyperfine's wall-clock mean.

### Connection setup cost

Each iteration = one connect + one call + one disconnect. Subtract the
per-op round-trip latency from the previous bench to isolate the connect
cost.

```
hyperfine --warmup 3 --runs 10 \
    'target/release/bench-wirebus-connect 2000' \
    'target/release/bench-dbus-connect 2000'
```

DBus pays the SASL AUTH EXTERNAL + Hello handshake on every new
connection; rev's current protocol has none of that, so the gap is
large here — that's expected, not a bug.

### Sanity: per-op latency cross-check

Run one binary stand-alone to see its per-op number:

```
target/release/bench-wirebus-rtt 50000
# stderr: wirebus-rtt: 50000 iters in 87.12 ms  (1.74 µs/op, 573978 op/s)
```

If the stderr line's µs/op is wildly different from `hyperfine mean / N`,
something's off (e.g., hyperfine running the binary from a cold cache or
the socket path mismatch).

## Caveats — read before drawing conclusions

1. **Scope mismatch.** DBus does introspection, typed signatures,
   policy checks, and name-ownership. WireBus is a thin
   length-prefixed MessagePack frame with no validation. Raw latency
   numbers can look very different without either being "better".
2. **System load.** Hyperfine's warmup handles hot-cache effects but a
   busy machine will inflate variance. `--runs 20+` helps; pinning
   the bench to one CPU (`taskset -c 3 hyperfine …`) helps more.
3. **Session bus vs system bus.** The DBus bench hits the user's
   session bus — short path, same UID. A system-bus comparison
   (e.g. `systemd-logind`) pays extra policy cost and isn't apples-to-
   apples with this bench.
4. **Mode matters.** Binary built with `--release` (this is the
   default in `Cargo.toml` here with `lto = "fat"`). Debug builds
   will show `rmp-serde` allocations as 3–5× slower — don't mix.
5. **Nagle / buffering.** Unix domain sockets don't have Nagle, but
   kernel scheduling still matters for tight ping-pong benchmarks.
   Latencies under ~2 µs are close to the syscall floor and mostly
   measure `read`/`write` syscall entry cost.

## What this measures — and what it doesn't

| Operation | Bench | Fair? | Notes |
|-----------|-------|-------|-------|
| Empty round-trip | `wirebus-rtt` vs `dbus-rtt` | ✅ | Both hit the broker, not a user service. |
| Connection setup | `wirebus-connect` vs `dbus-connect` | ✅ | DBus does auth; rev does not. Gap expected. |
| FD passing | *todo* | — | `OpenDevice` vs `systemd-logind.TakeDevice`. |
| Service start | *todo* | — | `rev start X` vs `systemctl start X`. |

## Adding a bench

Drop a new file under `src/bin/bench-foo.rs`, add a `[[bin]]` entry to
`Cargo.toml`. Keep the shape:

```rust
use clap::Parser;
use rev_benchmarks::timing::time_iters;

#[derive(Parser)] struct Args {
    #[arg(default_value_t = 10_000)] iters: u64,
}

fn main() -> ... {
    let args = Args::parse();
    /* warm-up: one real op */
    time_iters("bench-name", args.iters, || {
        for _ in 0..args.iters { /* do the op */ }
    });
    Ok(())
}
```

The `time_iters` helper prints the internal-measurement line to stderr.
Stdout stays clean so hyperfine's `--show-output` doesn't drown.

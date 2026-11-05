# Embedded-HAL & Embassy Compatibility Audit

**Status:** paper-only (P8.3, 2026-04-20). ADR-0022.
**Scope:** can `umwelt-core` and `umwelt-sep` run, unchanged, inside
an embedded Rust program built against the mainstream ecosystem —
`embedded-hal 1.0`, `embedded-io 0.6`, `embedded-io-async 0.6`,
Embassy (executor + time), and `lora-phy`?

## Executive summary

**Yes.** The runtime is already `no_std`-clean at the bar P8.1 proved,
and the SEP transport trait is blocking-poll with non-fatal
`WouldBlock` — the exact shape the embedded ecosystem already uses
for link-layer I/O. No structural changes are required for v0.1.

Downstream integrators write a short adapter (~50 lines for a framed
UART, ~30 lines for a `lora-phy` radio driver) in their port crate.
That adapter lives **outside this repo**, per P8's non-goal on
hardware-specific board-support crates.

This document pins the compatibility surface, enumerates the adapter
patterns, and records the forward-looking risks the next port crate
will hit first.

## Current public surfaces

### `umwelt-sep::SepTransport` — blocking, `no_std`

```rust
// crates/umwelt-sep/src/lib.rs
pub trait SepTransport {
    fn form(&self) -> FrameForm;
    fn send(&mut self, frame: &[u8]) -> Result<(), SepTransportError>;
    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, SepTransportError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SepTransportError {
    BufferTooSmall, WouldBlock, Closed, InvalidFrameLen, Io,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameForm { Inner /* 24 B */, Envelope /* 32 B */ }
```

Key properties:

- `&mut self` — single-owner. Fits the standard embedded pattern of
  "one peripheral per `Mutex<CriticalSection, RefCell<_>>>` or one
  peripheral per executor task."
- Non-blocking by convention: `recv` returns `WouldBlock` rather than
  parking. Callers pace themselves.
- Frame-oriented (not byte-oriented). One call = one SEP frame.
- Errors are `Copy` and carry no allocated payload — `no_std`-safe.

### `umwelt-core::SemioticPipeline<C,W,B>` — all-sync, tick-driven

Const-generic sized (channels × window × bins). No I/O of its own.
Consumes `Q15` samples, emits `PipelineEvent` into a caller-supplied
collector. Already no_std-clean (P8.1). Embedded integration is a
matter of plumbing ADC / I²C / SPI samples into `.observe(...)`.

## Target ecosystem surface

### `embedded-hal 1.0` — peripheral abstractions

Traits: `digital::OutputPin`, `digital::InputPin`, `spi::SpiBus`,
`i2c::I2c`, `delay::DelayNs`. Peripheral-side, not link-side.

**Relevance to us:** none directly. Radio drivers (`lora-phy`,
`sx126x-rs`, `llcc68`, …) consume `SpiBus + OutputPin + DelayNs` and
*expose* a higher-level tx/rx API that maps to `SepTransport`.

### `embedded-io 0.6` — byte-stream traits

```rust
pub trait ErrorType { type Error: Error; }
pub trait Read: ErrorType { fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>; }
pub trait Write: ErrorType {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error>;
    fn flush(&mut self) -> Result<(), Self::Error> { Ok(()) }
}
pub trait ReadReady: ErrorType { fn read_ready(&mut self) -> Result<bool, Self::Error>; }
```

**Relevance to us:** UART / USB-serial / TCP-on-embedded. If the
physical link to the radio is a UART, we adapt with a length-prefix +
CRC framer (details below).

### `embedded-io-async 0.6` — async variants

Same shape as above with `async fn` on `Read::read` / `Write::write`.
Embassy drivers expose these; `embassy-net` exposes them for TCP.

**Relevance to us:** our trait is blocking, but our `WouldBlock`
convention lets a caller write a tiny `async fn` wrapper that yields
to the executor between retries.

### `embassy-executor 0.6` + `embassy-time 0.3`

Embassy executes `#[embassy_executor::task]`-annotated functions.
`Timer::after_millis(n).await` yields cooperatively.

**Canonical SEP task pattern:**

```rust
// pseudocode — lives in the port crate, NOT here
#[embassy_executor::task]
async fn sep_io_task(mut transport: impl SepTransport + 'static) {
    let mut buf = [0u8; SEP_ENVELOPE_BYTES];
    loop {
        match transport.recv(&mut buf) {
            Ok(n)                                => dispatch(&buf[..n]),
            Err(SepTransportError::WouldBlock)   => Timer::after_millis(1).await,
            Err(SepTransportError::Closed)      => break,
            Err(_e)                              => Timer::after_millis(10).await,
        }
    }
}
```

Ten lines. No trait changes required.

### `lora-phy` — de-facto LoRa radio abstraction

`LoRa<R>` exposes `prepare_for_tx` / `tx(payload: &[u8])` and
`prepare_for_rx` / `rx(&mut buf, timeout)`. Frame-level. Maps to
`SepTransport` one-to-one.

### Board-level HALs

`esp-hal`, `nrf-hal`, `stm32-hal` and their Embassy counterparts
(`embassy-stm32`, `embassy-nrf`, `esp-hal-embassy`) expose
`embedded-hal 1.0` + `embedded-io[-async]` impls for their
peripherals. Consumers write target-specific glue in their own port
crates — per ADR-0022 those crates are **not** in this workspace.

## Compatibility matrix

| Ecosystem crate | Target version | Verdict | Action (in port crate) |
|---|---|---|---|
| `embedded-hal` | 1.0 | **compatible** | peripheral plumbing only (no SEP trait impl) |
| `embedded-io` | 0.6 | **compatible via framer** | length-prefix + CRC16 framer, ~50 lines |
| `embedded-io-async` | 0.6 | **compatible via async-wrap** | `async fn` wrapper around blocking `recv` |
| `embassy-executor` | 0.6 | **compatible** | spawn task, pattern above |
| `embassy-time` | 0.3 | **compatible** | `Timer::after_millis(1)` on `WouldBlock` |
| `embassy-net` | 0.4 | **compatible via framer** | TCP as byte stream, reuse UART framer |
| `lora-phy` | 3.x | **compatible, direct** | trait impl, ~30 lines |
| `esp-hal` / `esp-hal-embassy` | 0.19 / 0.3 | **compatible via above** | — |
| `nrf-hal` / `embassy-nrf` | 0.17 / latest | **compatible via above** | — |
| `stm32-hal` / `embassy-stm32` | 0.x / latest | **compatible via above** | — |
| `defmt` | 0.3 | **orthogonal** | log route for integrators; not used by our crates |

No row lands at "incompatible." One row ("compatible, direct") means
zero adapter code; all others mean <60 lines of port-crate glue.

## Adapter pattern #1 — `lora-phy` radio driver

```rust
// pseudocode — lives in umwelt-esp32 / umwelt-nrf / etc.
use lora_phy::{LoRa, mod_params::PacketParams, RxMode};
use umwelt_sep::{SepTransport, SepTransportError, FrameForm, SEP_ENVELOPE_BYTES};

pub struct LoRaSepAdapter<R: lora_phy::RadioKind> {
    lora: LoRa<R>,
    rx_params: PacketParams,
}

impl<R: lora_phy::RadioKind> SepTransport for LoRaSepAdapter<R> {
    fn form(&self) -> FrameForm { FrameForm::Envelope }

    fn send(&mut self, frame: &[u8]) -> Result<(), SepTransportError> {
        if frame.len() != SEP_ENVELOPE_BYTES {
            return Err(SepTransportError::InvalidFrameLen);
        }
        self.lora.tx(frame).map_err(|_| SepTransportError::Io)
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, SepTransportError> {
        if buf.len() < SEP_ENVELOPE_BYTES {
            return Err(SepTransportError::BufferTooSmall);
        }
        match self.lora.rx(&self.rx_params, buf, RxMode::Single) {
            Ok((len, _status)) if len as usize == SEP_ENVELOPE_BYTES => Ok(len as usize),
            Ok(_)                                                    => Err(SepTransportError::InvalidFrameLen),
            Err(lora_phy::mod_traits::RadioError::ReceiveTimeout)    => Err(SepTransportError::WouldBlock),
            Err(_)                                                   => Err(SepTransportError::Io),
        }
    }
}
```

`lora-phy` already enforces SEP's one-frame-per-call contract. The
adapter is a translation table over errors; no buffering.

## Adapter pattern #2 — `embedded-io` byte stream (framed UART)

```rust
// pseudocode — lives in the port crate
use embedded_io::{Read, Write};
use umwelt_sep::{crc16_ccitt, SepTransport, SepTransportError, FrameForm, SEP_ENVELOPE_BYTES};

const OVERHEAD: usize = 1 /* len byte */ + 2 /* crc16 */;
const FRAMED:   usize = OVERHEAD + SEP_ENVELOPE_BYTES; // 35

pub struct FramedUartSepAdapter<U> {
    uart: U,                          // embedded_io::Read + Write
    rx_scratch: heapless::Vec<u8, { FRAMED * 2 }>,
}

impl<U: Read + Write> SepTransport for FramedUartSepAdapter<U> {
    fn form(&self) -> FrameForm { FrameForm::Envelope }

    fn send(&mut self, frame: &[u8]) -> Result<(), SepTransportError> {
        if frame.len() != SEP_ENVELOPE_BYTES {
            return Err(SepTransportError::InvalidFrameLen);
        }
        let mut out = [0u8; FRAMED];
        out[0] = frame.len() as u8;
        out[1..1 + frame.len()].copy_from_slice(frame);
        let crc = crc16_ccitt(&out[..1 + frame.len()]);
        out[1 + frame.len()..].copy_from_slice(&crc.to_le_bytes());
        self.uart.write_all(&out).map_err(|_| SepTransportError::Io)
    }

    fn recv(&mut self, buf: &mut [u8]) -> Result<usize, SepTransportError> {
        if buf.len() < SEP_ENVELOPE_BYTES {
            return Err(SepTransportError::BufferTooSmall);
        }
        // 1. Pump bytes into rx_scratch (non-blocking).
        let mut scratch = [0u8; 32];
        match self.uart.read(&mut scratch) {
            Ok(0) | Err(_) => {}
            Ok(n) => { let _ = self.rx_scratch.extend_from_slice(&scratch[..n]); }
        }

        // 2. Need at least one full framed packet.
        if self.rx_scratch.len() < FRAMED {
            return Err(SepTransportError::WouldBlock);
        }
        if self.rx_scratch[0] as usize != SEP_ENVELOPE_BYTES {
            // Resync by discarding one byte; real impl would scan for a preamble.
            self.rx_scratch.remove(0);
            return Err(SepTransportError::WouldBlock);
        }

        // 3. Verify CRC, split-off one frame, emit.
        let slice = &self.rx_scratch[..FRAMED];
        let body = &slice[1..1 + SEP_ENVELOPE_BYTES];
        let crc_rx = u16::from_le_bytes([slice[FRAMED - 2], slice[FRAMED - 1]]);
        let crc_calc = crc16_ccitt(&slice[..1 + SEP_ENVELOPE_BYTES]);
        if crc_rx != crc_calc {
            self.rx_scratch.remove(0);
            return Err(SepTransportError::WouldBlock);
        }
        buf[..SEP_ENVELOPE_BYTES].copy_from_slice(body);
        self.rx_scratch.drain(..FRAMED);
        Ok(SEP_ENVELOPE_BYTES)
    }
}
```

~50 lines, `no_std`, zero heap. `crc16_ccitt` is re-exported from
`umwelt-sep` (same CRC our on-wire format uses for the inner frame —
ADR-0011), so the framer and the SEP decoder share one implementation.

## Adapter pattern #3 — Embassy async wrapper around blocking `recv`

```rust
// pseudocode — lives in the port crate
use embassy_time::Timer;
use umwelt_sep::{SepTransport, SepTransportError};

pub async fn recv_async<T: SepTransport>(
    t: &mut T,
    buf: &mut [u8],
) -> Result<usize, SepTransportError> {
    loop {
        match t.recv(buf) {
            Ok(n)                                   => return Ok(n),
            Err(SepTransportError::WouldBlock)      => Timer::after_millis(1).await,
            Err(e)                                  => return Err(e),
        }
    }
}
```

Ten lines. The `1 ms` quantum is tunable per radio; low-duty-cycle
LoRa deployments may prefer `Timer::after_millis(10)` or even
`100`. This is an integrator choice, not a core-crate concern.

## Risks and watch-items

1. **`embedded-hal 2.0` drift.** Not yet published but under
   discussion on the HAL tracker. If trait splits change materially,
   port-crate adapters rebuild against new signatures. `SepTransport`
   is insulated because we never imported `embedded-hal` types into
   the trait surface.
2. **Async-only radio drivers.** A driver that publishes only
   `async fn tx(...)` (no blocking variant) forces `block_on` inside
   a `SepTransport::send` impl, which is incompatible with Embassy's
   cooperative executor. Mitigation: add a `SepTransportAsync` trait
   in a future phase. Not warranted today — `lora-phy` ships both.
3. **Multi-radio nodes.** `&mut self` means one `SepTransport` per
   radio. Consumers with two radios (e.g. LoRa + BLE for dual-range)
   run two transports and a multiplexer. Standard pattern, no
   trait change needed.
4. **`async fn` in traits stability.** Our MSRV is 1.85; `async fn in
   traits` is stable since 1.75, so a future `SepTransportAsync` will
   compile without nightly features. We just don't need it yet.
5. **`defmt` vs `tracing`.** Our crates use `tracing` (host-only in
   `umwelt-cli`). Embedded integrators will prefer `defmt` for link
   logs; our `no_std` crates do no logging, so there's no clash.
6. **Critical-section / `portable-atomic`.** `umwelt-core` relies on
   no atomics today. `umwelt-sep`'s replay window uses
   `heapless::FnvIndexSet` which is lock-free and works on any
   embedded target. No `portable-atomic` shim needed.
7. **Heap absence.** Neither core nor sep allocates. Port crates that
   feature-gate their own heap use (`alloc::Vec` for formatted
   dashboards, etc.) will not interact with our crates.

## What P8.3 does **not** answer

- Power-consumption budget of the pipeline on a real ESP32-S3.
  That's P8-awaiting-hardware.
- Interrupt-context safety of specific port-crate adapter
  implementations. Adapter authors own that analysis.
- Whether the pipeline + transport + radio fits on 128 KB SRAM
  together. That's an integration measurement, not a trait audit,
  and lives in the first port crate.

## Conclusion

The public API is embedded-ready. The remaining work to put a
`SemioticPipeline` on an ESP32-S3 or nRF52 belongs in a downstream
port crate — ~60-100 lines of adapter code plus the integrator's
board wiring. Nothing in this repo blocks it.

When the first port crate lands, it may surface one follow-up trait
addition (`SepTransportAsync`) or may confirm that the async-wrap
pattern is sufficient. Either outcome is compatible with the v0.1
shape pinned here.

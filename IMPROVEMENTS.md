# Screx Performance Improvement Plan

Scope: **Linux daemon** and **iPad app**. The desktop client is intentionally out of
scope for this plan.

Goal: reduce glass-to-glass latency and per-packet CPU overhead, and remove the
failure modes where latency *creeps up* and never recovers. Items are grouped into
phases; within each phase they are ordered by impact. Each item states what is wrong,
how to fix it, and what it buys.

Measurement baseline (do this before Phase 1): record end-to-end latency with a
timer-on-screen photo test (phone camera pointed at both displays), note daemon CPU
per frame (`perf top` on the capture thread), and log frame intervals on the iPad
(`CACurrentMediaTime()` deltas at enqueue). Re-measure after each phase so wins are
attributable.

---

## Phase 0 — Restore the build and finish adaptive bitrate (daemon)

### 0.1 Fix the broken build on `main`

`cargo check` fails with 5 errors. `daemon/src/main.rs` references three things that
don't exist:

- `capture::CaptureBackend` and a `backend` field on `CaptureConfig`
  (`main.rs:352`) — `capture.rs` only implements EVDI and its `CaptureConfig` is
  `{width, height, fps}`.
- `Encoder::bitrate_bps()` and `Encoder::reconfigure_bitrate()`
  (`main.rs:484-488`).

**Steps**

1. Add `pub enum CaptureBackend { Evdi }` (room for `Vkms` later) and a
   `backend: CaptureBackend` field to `CaptureConfig` in `capture.rs`; plumb it
   through `Capture::new` (EVDI is the only arm for now).
2. Add to `encode::Encoder`:
   - `pub fn bitrate_bps(&self) -> u32` returning the currently configured bitrate
     (store it on the struct when configuring).
   - `pub fn reconfigure_bitrate(&mut self, bps: u32) -> Result<()>`.

### 0.2 Implement in-place bitrate reconfiguration (do NOT rebuild the encoder)

The adaptive path in `stream_server.rs` (`AdaptiveStreamState`) computes a target
bitrate under loss/PLI, but today only the FEC percentage actually adapts. The
encoder keeps pushing the full bitrate into a degraded link — the worst possible
response to congestion.

**Steps**

1. In `reconfigure_bitrate`, update the live codec context without teardown:
   - set `(*ctx).bit_rate = bps as i64`
   - set `(*ctx).rc_max_rate = bps as i64`
   - resize `rc_buffer_size` consistently (see 1.4)
   - for VAAPI, ffmpeg picks up `bit_rate` changes on the fly for CBR/VBR; verify
     with `vainfo`-supported rate control. If the driver ignores it, fall back to
     `av_opt_set_int(ctx.priv_data, "b", ...)` and, only as a last resort, a
     keyframe-aligned encoder rebuild.
2. Never reuse the resolution-change path (`encode.rs:150-161`) for bitrate — a full
   hw-context rebuild stalls the stream for hundreds of ms.
3. Verify: throttle the link (`tc qdisc add dev ... netem loss 5%`), confirm the
   encoder's output bitrate drops within ~1s and recovers afterwards.

**Why it helps:** under Wi-Fi loss, lowering source bitrate is the mechanism that
actually reduces queueing and re-loss; FEC alone adds overhead to an already
saturated link. This is the single largest missing performance feature.

---

## Phase 1 — Daemon hot-path fixes (video pipeline)

### 1.1 Eliminate the CPU color conversion + double copy per frame — HIGH

`encode.rs:527-556` (`HwEncoder::push_frame`): every frame does a single-threaded
`sws_scale` BGRA→NV12 on the CPU into `sw_nv12`, then `av_hwframe_transfer_data`
copies that buffer to the GPU surface. At the default 2160×1620 this is ~14 MB
converted + ~5 MB copied per frame, on the same thread that also captures and sends.

**Steps (preferred, option A — GPU conversion):**

1. Create the VAAPI hwframes context with `AV_PIX_FMT_BGRA` as the sw format and
   upload the captured BGRA buffer directly with `av_hwframe_transfer_data`
   (one copy, no conversion).
2. Insert a VAAPI VPP conversion to NV12 before the encoder: either via an
   `AVFilterGraph` (`hwupload,scale_vaapi=format=nv12`) or by allocating a second
   NV12 hwframes ctx and using `av_hwframe_transfer_data`/VPP blit between surfaces.
3. Keep the current path as fallback behind a flag for drivers with broken VPP.

**Steps (option B — smaller change, removes one copy):**

1. `av_hwframe_get_buffer` as today, then `av_hwframe_map(mapped, hw_frame,
   AV_HWFRAME_MAP_WRITE | AV_HWFRAME_MAP_OVERWRITE)`.
2. Point `sws_scale`'s destination at the mapped frame's `data`/`linesize`, so the
   NV12 conversion writes straight into the GPU-visible surface; drop `sw_nv12`
   entirely.
3. Unmap before `avcodec_send_frame`.

**Why it helps:** removes the largest fixed CPU cost per frame; frees the capture
thread (see 1.2) and cuts several ms/frame at high resolutions — direct
glass-to-glass latency reduction.

### 1.2 Unserialize capture → encode → FEC → send — HIGH

`main.rs:376-538`: the entire pipeline runs synchronously inside the EVDI `on_frame`
callback on one thread, and `capture.rs:373-397` registers only **one** EVDI pixel
buffer. The compositor cannot hand over frame N+1 while the daemon is still
encoding/FEC-ing/sending frame N. VAAPI `async_depth=1` (`encode.rs:405-406`)
serializes GPU encode on top.

**Steps**

1. Register two EVDI buffers (ids 0 and 1) and ping-pong them: request the next
   grab into buffer B while buffer A is being encoded.
2. Split the network tail off the capture thread: after `drain_packets`, hand the
   encoded access unit to a dedicated sender thread via a **bounded channel of
   capacity 1 with drop-oldest semantics** (`crossbeam` `ArrayQueue` or a
   mutex+condvar slot). If the sender is still busy (network backpressure), the
   old AU is replaced — never queue video.
   - Exception: never drop an IDR in favor of a non-IDR; if an IDR is pending,
     drop the newer P-frame instead or force the next frame to be an IDR.
3. FEC encoding (Reed-Solomon) moves to the sender thread along with `sendmmsg`.
4. Leave `async_depth=1` for now (it bounds encoder latency); revisit only if
   profiling shows GPU idle bubbles after steps 1–3.

**Why it helps:** capture cadence stops being gated by network syscalls and FEC
math. Under load this is the difference between "fps degrades and latency grows"
and "fps holds, one frame drops".

### 1.3 Remove the per-packet heap allocation in the UDP send path — HIGH

`stream_server.rs:130-152`: since the source-IP pinning commit,
`build_pktinfo_cmsg` allocates a fresh `Vec<u8>` for **every outgoing UDP packet**
(every 1400-byte video chunk, every audio chunk).

**Steps**

1. `CMSG_SPACE(size_of::<in_pktinfo>())` is a constant. Replace the `Vec` with a
   fixed `[u8; PKTINFO_CMSG_SPACE]` built once per session (the source IP only
   changes on reconnect), stored on `UdpSender`/`AudioSender`.
2. `send_to_from` takes `&[u8]` for the cmsg instead of `Vec<u8>`.

**Why it helps:** removes a malloc/free per packet on the hottest send path —
thousands of allocations/sec at 60 fps gone with a ~20-line change.

### 1.4 Size the VBV buffer for latency, not for quality smoothing — MEDIUM

`encode.rs:400` and `:706`: `rc_buffer_size = bitrate/2` ≈ 500 ms of bits. This
lets the encoder emit bursts far above the per-frame budget; big bursts mean more
UDP chunks, more FEC shards, and network queueing.

**Steps**

1. Set `rc_buffer_size = bitrate_bps / fps * 2` (≈ 2 frame intervals) and
   `rc_max_rate = bitrate_bps` for both HW and SW encoders.
2. Keep this consistent inside `reconfigure_bitrate` (0.2).
3. Watch for quality pumping on scene cuts; if objectionable, relax to 3–4 frame
   intervals — still 10× tighter than today.

**Why it helps:** caps worst-case frame size, which caps worst-case send/transmit
time per frame — tighter latency tail, fewer FEC-heavy mega-frames.

### 1.5 Stop copying every encoded access unit — MEDIUM

`encode.rs:592-599` and `:846-853`: every AU is `to_vec()`'d out of the `AVPacket`.

**Steps**

1. Add an `OwnedPacketBuf` wrapper holding an `AVBufferRef*` obtained via
   `av_buffer_ref((*pkt).buf)`, with `Deref<Target=[u8]>` and a `Drop` impl calling
   `av_buffer_unref`.
2. Return that from `drain_packets` instead of `Vec<u8>`; keep the copy+concat
   path only for the IDR-with-extradata-prefix case.

**Why it helps:** removes an allocation + full-frame memcpy per encoded frame
(hundreds of KB on IDRs).

---

## Phase 2 — Daemon audio, input, and USB transport

### 2.1 Make audio start/stop event-driven — HIGH (known problem area)

`audio.rs:187-197` and `:301-307`: the capture loop polls readiness every 200 ms
and imposes a flat 500 ms sleep after every stop, plus a fresh `parec` spawn each
time. Toggling the speaker can cost up to ~1 s before audio resumes.

**Steps**

1. Replace the 200 ms poll with a `Condvar` (or `parking_lot` condvar) notified by
   the `SPKR` handler and session connect/disconnect paths (they already flip the
   relevant atomics).
2. Make the 500 ms backoff conditional: only apply it when `parec` exited with an
   error (crash-loop protection); on a clean, intentional stop, loop immediately
   back to the wait.
3. (Stretch) keep `parec` alive across speaker toggles and gate sending instead of
   killing the process — removes the PulseAudio renegotiation cost entirely.

**Why it helps:** speaker re-enable drops from ~700–1000 ms to effectively
instant; this matches the audio-latency symptoms in recent fix commits.

### 2.2 Move blocking unicode typing off the input-dispatch thread — HIGH

`uinput.rs:1034-1078` (`type_unicode`, sleep at line 1046): a 10 ms
`thread::sleep` per non-ASCII character runs on the same thread that dispatches
ALL control messages (touch, mouse, key, gamepad) — confirmed for both the network
control loop (`pairing.rs:553-651`) and USB (`usb.rs:340-406`).

**Steps**

1. Add a small keyboard worker thread owning `VirtualKeyboard`, fed by a bounded
   channel of key events from `handle_key_packet`.
2. Touch/mouse/gamepad stay on the dispatch thread (they must not queue behind
   typing).
3. Alternative minimal fix if a worker feels heavy: batch the IBus sequence and
   reduce the sleep, but the worker is the correct fix — any future slow input
   handling lands there for free.

**Why it helps:** pasting or typing non-ASCII text no longer freezes cursor and
touch response in 10 ms increments.

### 2.3 Batch uinput writes — MEDIUM

`uinput.rs` emit helpers (`:304-318`, `:449-513`, `:605-651`, `:1134-1164`): one
`write()` syscall per 16-byte `input_event`. A mouse move is 3 syscalls; a gamepad
state update is up to ~19.

**Steps**

1. Add an `emit_batch(&mut self, events: &[input_event])` helper that writes all
   events (including the trailing `SYN_REPORT`) in a single `write_all` — the
   uinput kernel driver accepts multiple events per write.
2. Convert `move_rel`, `set_state`, `handle_touch_packet` per-contact emission,
   and the key press/release pairs to build an on-stack array
   (`arrayvec`/`SmallVec` or a fixed `[input_event; N]`) and call `emit_batch`.

**Why it helps:** 3–19× fewer syscalls on the input path; lower input latency
jitter at high event rates (trackpad, gamepad polling).

### 2.4 Stop copying whole video frames into the USB write buffer — MEDIUM

`usb.rs:79-99` (`send_video`): the entire annex-B frame (hundreds of KB for IDRs)
is memcpy'd into `write_buf` before `write_all`; frames over the 256 KB reserve
also force a `Vec` regrow.

**Steps**

1. Build only the 11-byte header in `write_buf`, then send with
   `write_vectored(&[IoSlice::new(&header), IoSlice::new(annex_b)])`, looping on
   partial writes (or simply two `write_all` calls — the stream already has
   TCP_NODELAY semantics via framing, and a 2-syscall send of header+payload is
   fine here).
2. Apply the same pattern to `send_audio` for consistency.

**Why it helps:** USB is the designated low-latency transport; this removes a
full-frame copy on it, largest exactly on IDR frames.

### 2.5 Small daemon cleanups — LOW (batch these opportunistically)

- `camera.rs:80`: the 500 ms sleep in `create_cam_writer` runs on the
  control-message thread (via `enable_camera`). Spawn the webcam setup on its own
  thread; touch/key/mouse must not stall behind it.
- `stream_server.rs:1261`: `std::env::var_os("SCREX_LOG_SENT_AUS")` per frame →
  read once in `UdpSender::new`, store a `bool`.
- `stream_server.rs:1221`: bound the `rs_cache` (e.g. clear when > 64 entries).
- `audio.rs:339-360`: replace the per-sample `to_le_bytes` loop with
  `bytemuck::cast_slice` on little-endian targets.
- Reduce repeated `Mutex` acquisitions per UDP packet in `run_client_manager`
  (lock `client_addr` once per packet, reuse the copy).
- Opportunistically raise the capture/encode/send thread priority
  (`SCHED_FIFO` low prio or `nice -10`) with graceful fallback when unprivileged —
  complements the existing DSCP/`SO_PRIORITY` socket tuning.

---

## Phase 3 — iPad app

### 3.1 Kill the per-packet main-actor hop for traffic counters — HIGH

`ScrexApp.swift:704-708`, `892-898`, `972-978` + `recordTraffic` at `:624-631`:
every UDP datagram, USB frame, and TCP control frame spawns
`Task { @MainActor in ... }` just to add to two byte counters that a 1 Hz timer
reads. At 60 fps video chunked at 1400 B this is hundreds to >1000 main-actor
tasks per second competing with SwiftUI rendering and gesture handling.

**Steps**

1. Add a tiny `TrafficCounter` class with two atomics
   (`ManagedAtomic<UInt64>` from swift-atomics, or `OSAllocatedUnfairLock` around
   two `UInt64`s).
2. `onTraffic` callbacks call `counter.add(rx:tx:)` directly on whatever thread
   they fire on — no Task, no actor hop.
3. The existing 1 Hz `startTrafficMonitoring` timer reads-and-resets the atomics
   and updates the `@Published` UI values once per second.

**Why it helps:** removes the single largest source of main-thread churn in the
app; directly reduces frame hitches under high packet rates. Easiest high-impact
fix in this plan.

### 3.2 Fix O(n²) receive-buffer compaction on the USB path — HIGH

`USBListener.swift:180-201`: each parsed message does
`Data(recvBuffer.prefix(...).dropFirst(4))` + `recvBuffer = Data(recvBuffer.dropFirst(totalNeeded))`
— two copies per message, and the remainder copy is O(bytes still queued). When a
256 KB read contains several frames back-to-back (catch-up after a stall), this
goes quadratic on the low-latency transport.

**Steps**

1. Keep `recvBuffer` but track a `readOffset: Int` alongside it. Parse messages by
   slicing at `readOffset` without reassigning the buffer.
2. Compact once per `receive()` callback (not per message): if
   `readOffset > some threshold (e.g. 64 KB)`, do a single
   `recvBuffer.removeSubrange(0..<readOffset)` and reset the offset.
3. Extract payloads with `recvBuffer.subdata(in:)` only for the final message
   `Data` handed downstream (one copy, unavoidable given ownership).
4. Apply the same pattern to `NetworkControlClient.swift:127-146` (lower rate,
   same anti-pattern).

**Why it helps:** turns worst-case per-read cost from O(n²) to O(n); smooths
exactly the catch-up-after-stall moments where latency is already stressed.

### 3.3 Make the audio render callback lock-free — HIGH (glitch risk)

`AudioPlayer.swift:78-110` + `enqueueAudio` at `:147-185`: the
`AVAudioSourceNode` render block runs on a real-time audio thread and takes an
`NSLock` contended by the network thread. If the network thread is preempted
while holding it, the render deadline is missed → audible glitch (priority
inversion).

**Steps**

1. Replace the ring bookkeeping with a lock-free SPSC ring buffer: fixed
   `UnsafeMutableRawPointer` storage, `ManagedAtomic<Int>` head (written only by
   the render thread) and tail (written only by the network thread), each side
   reading the other side's index with acquire ordering.
2. Producer (`enqueueAudio`): compute free space from the cached head; on overflow
   keep the current drop-oldest policy by advancing… **no** — a pure SPSC ring
   cannot have the producer move the consumer index; instead drop the *incoming*
   packet on overflow and count it (overflow already implies latency has built up,
   and the AV-drift logic can request a resync).
3. Consumer (render block): if available < needed, output silence for the
   shortfall (as today) — never block, never allocate, never take a lock.
4. Keep buffer size math identical so this is a pure mechanism swap.

**Why it helps:** removes the only priority-inversion hazard in the audio path;
audio dropouts under CPU load stop being possible from this cause.

### 3.4 Reduce per-chunk crypto allocation churn — MEDIUM

`Crypto.swift:9-36`, `:89-133`, called per UDP chunk from
`StreamClient.swift:269` (receive) and `:130` (send): each chunk allocates a fresh
12-byte nonce `Data`, converts to `AES.GCM.Nonce` (another copy), and
`seal`/`open` results get concatenated into yet another `Data`.

**Steps**

1. Build nonces with `withUnsafeTemporaryAllocation(byteCount: 12)` (or a reusable
   12-byte buffer per direction — nonce contents are seq-derived, so a single
   scratch buffer per cipher direction is safe on the single receive queue).
2. On the seal path, pre-size the output `Data` once
   (`ciphertext.count + 16`) and append tag into it, avoiding the intermediate.
3. Measure before going further: if Instruments still shows CryptoKit allocation
   overhead dominating, drop to CommonCrypto/BoringSSL-style one-shot AES-GCM into
   caller-provided buffers. Don't do this preemptively — CryptoKit's AES-GCM is
   hardware-backed and the win may already be sufficient after steps 1–2.

**Why it helps:** dozens of allocations per video frame removed on the same hot
path as 3.1/3.2 — the three compound.

### 3.5 Add display-layer backpressure — MEDIUM

`Decoder.swift:313-331`: `displayLayer.enqueue(sampleBuffer)` is called
unconditionally. After a stall (backgrounding, thermal throttle, network
catch-up), buffers queue in the layer and then play out as a fast-forward burst
instead of snapping to live.

**Steps**

1. Before enqueue, check `displayLayer.isReadyForMoreMediaData`. If not ready:
   drop the frame **unless** it is the parameter-set/IDR-bearing buffer for a new
   sync point; track that we dropped and request a PLI if more than N consecutive
   frames were dropped.
2. Since frames are tagged `kCMSampleAttachmentKey_DisplayImmediately`, dropping
   stale non-IDR frames is safe for display but not for decode continuity — with
   an `AVSampleBufferDisplayLayer` doing the decoding, dropped P-frames would
   corrupt the stream. So the correct policy is: when the layer is not ready,
   `flush()` the layer and request a PLI (fresh IDR) rather than dropping
   individual frames. This trades one keyframe request for an instant return to
   live video.
3. Rate-limit the flush+PLI (e.g. at most once per second).

**Why it helps:** converts "stall → minutes of trailing latency / fast-forward"
into "stall → one keyframe → live again".

### 3.6 Cheaper FEC recovery — MEDIUM (fires exactly when the network is bad)

`FEC.swift:10-83` (`recover`), `:194-241` (`invertMatrix`): every loss event
re-inverts the decode matrix (O(k³)) and copies every shard `Data → [UInt8] →
Data`.

**Steps**

1. Cache inverted sub-matrices keyed by `(dataCount, parityCount, usedIndices)` —
   the key space is small for real shard counts; an LRU of ~32 entries covers
   recurring loss patterns.
2. Rework the GF math to run over `Data` via
   `withUnsafeBytes`/`withUnsafeMutableBytes` instead of materializing `[UInt8]`
   arrays per shard.

**Why it helps:** cuts CPU spent per recovery event severalfold, precisely in the
degraded-network moments where the CPU budget is already strained by retransmit
pressure and PLI-triggered IDR decodes.

### 3.7 Small iPad cleanups — LOW

- `StreamClient.swift:309` + `:368-386`: `pruneOldFrames()` runs per received
  chunk; run it once per completed frame or on a 250 ms timer instead.
- `MicCapture.swift:108-120`: `sampleAccumulator.removeFirst(opusFrameSize)`
  shifts the array every 20 ms frame; reuse the ring-buffer approach from
  `AudioPlayer` or track a read offset.
- `DisplayView.swift:455-482`: touch packet building does many small `Data`
  appends per touch at up to 120 Hz; pre-size one `Data` per event batch.

### 3.8 Explicitly NOT planned (verified as already good)

- The `AVSampleBufferDisplayLayer` direct-decode design with
  `DisplayImmediately` and host-time PTS is the right low-latency pattern — keep.
- Hot-path logging is already throttled to warm-up windows; no change.
- Per-NALU `CMBlockBuffer` copy (`Decoder.swift:239-281`) is acceptable; only
  revisit (double-buffered custom allocator) if Instruments shows it after
  everything above ships.

---

## Suggested execution order

| Order | Item | Area | Effort | Impact |
|---|---|---|---|---|
| 1 | 0.1 + 0.2 build fix + live bitrate reconfig | daemon | S–M | unblocks everything; biggest lossy-network win |
| 2 | 3.1 traffic-counter atomics | iPad | S | high, trivially safe |
| 3 | 1.3 cmsg stack buffer | daemon | S | high, trivially safe |
| 4 | 2.1 event-driven audio start/stop | daemon | S–M | high (known pain point) |
| 5 | 3.2 USB recv offset parsing | iPad | M | high on USB path |
| 6 | 1.1 GPU color convert (option B first, A after) | daemon | M–L | biggest per-frame CPU win |
| 7 | 1.2 double-buffered capture + sender thread | daemon | L | latency stability under load |
| 8 | 3.3 lock-free audio ring | iPad | M | eliminates glitch class |
| 9 | 3.5 flush+PLI backpressure | iPad | S–M | eliminates latency-creep class |
| 10 | 2.2 keyboard worker thread | daemon | S | input latency under typing |
| 11 | 1.4 VBV sizing, 1.5 AU refcounting, 2.3 uinput batching, 2.4 USB vectored write, 3.4 crypto, 3.6 FEC | both | S–M each | steady accumulation |
| 12 | 2.5 + 3.7 cleanups | both | S | polish |

Effort: S < half a day, M ≈ a day, L = multi-day.

## Verification per phase

- **Phase 0:** `cargo check` clean; `tc netem` loss test shows bitrate adaptation.
- **Phase 1:** `perf` on the daemon shows `sws_scale`/`memcpy` gone or off the
  capture thread; frame interval stddev under `iperf3` background load improves.
- **Phase 2:** speaker toggle-to-audio time (log timestamps) < 100 ms; paste
  non-ASCII text while dragging the mouse — no cursor stutter.
- **Phase 3:** Instruments Time Profiler on the iPad: main thread time attributable
  to `Task`/actor machinery during streaming drops to noise; Allocations
  instrument shows per-second transient allocation count during streaming down by
  an order of magnitude; induced 3 s stall recovers to live video in ≤ 1 keyframe
  interval.

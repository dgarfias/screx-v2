import Foundation

// From client:
// swiftc Screx/AudioJitterBuffer.swift Tests/AudioJitterBufferTests.swift -o /tmp/screx-audio-tests && /tmp/screx-audio-tests
@main
struct AudioJitterBufferTests {
    static let a = UnsafeMutablePointer<UInt8>.allocate(capacity: AudioJitterBuffer.maxPacketBytes)
    static let b = UnsafeMutablePointer<UInt8>.allocate(capacity: AudioJitterBuffer.maxPacketBytes)

    /// Inserts packet `seq` (one byte: its low byte) with a timestamp of `seq * 10` ms.
    static func insert(_ buffer: AudioJitterBuffer, _ seq: Int64, at nowMs: Double, primary: Bool = true) {
        var byte = UInt8(truncatingIfNeeded: seq)
        withUnsafeBytes(of: &byte) { raw in
            buffer.insert(seq: seq, timestampMs: UInt32(seq * 10), isPrimary: primary, bytes: raw, nowMs: nowMs)
        }
    }

    static func pull(_ buffer: AudioJitterBuffer) -> AudioJitterBuffer.Action {
        buffer.pull(queuedMs: 0, a: a, b: b)
    }

    /// Asserts the next pull decodes packet `seq`.
    static func expectDecode(_ buffer: AudioJitterBuffer, _ seq: Int64, line: Int = #line) {
        let action = pull(buffer)
        precondition(action == .decode(1) && a.pointee == UInt8(truncatingIfNeeded: seq),
                     "line \(line): expected decode of \(seq), got \(action) (\(a.pointee))")
    }

    static func main() {
        payloadParsing()
        reportEncoding()
        waitsForTargetThenPlaysInOrder()
        concealsHolesAndDropsLatePackets()
        redundantCopiesFillHoles()
        underrunConcealsBrieflyThenRefills()
        mergesWhenFarTooDeep()
        drainsModerateExcessQuickly()
        newStreamForgetsTheTarget()
        resyncsAfterAJump()
        targetFollowsJitter()
        targetRespectsVideoCap()
        print("AudioJitterBufferTests passed")
    }

    static func payloadParsing() {
        let payload = Data([3, 0, 0, 2, 0xA, 0xB, 1, 0, 1, 0xC, 3, 0, 1, 0xD])
        let entries = SpeakerAudioPayload.parse(payload)!
        precondition(entries == [SpeakerAudioEntry(distance: 0, opus: 4..<6),
                                 SpeakerAudioEntry(distance: 1, opus: 9..<10),
                                 SpeakerAudioEntry(distance: 3, opus: 13..<14)])
        // Sliced Data keeps its own indices.
        let sliced = (Data([0xFF]) + payload)[1...]
        precondition(SpeakerAudioPayload.parse(sliced)!.first!.opus == 5..<7)
        precondition(SpeakerAudioPayload.parse(Data()) == nil)
        precondition(SpeakerAudioPayload.parse(Data([1, 1, 0, 1, 0xA])) == nil, "first must be distance 0")
        precondition(SpeakerAudioPayload.parse(Data([2, 0, 0, 1, 0xA, 0, 0, 1, 0xB])) == nil, "distances increase")
        precondition(SpeakerAudioPayload.parse(Data([1, 0, 0, 2, 0xA])) == nil, "truncated")
        precondition(SpeakerAudioPayload.parse(Data([1, 0, 0, 1, 0xA, 0])) == nil, "trailing bytes")
        precondition(SpeakerAudioPayload.parse(Data([2, 0, 0, 1, 0xA, 9, 0, 1, 0xB])) == nil, "distance > 8")
    }

    static func reportEncoding() {
        let report = AudioPlayoutReport(generation: 7, depthUs: 25_000, targetUs: 20_000, due: 20,
                                        primaryLost: 3, recovered: 2, concealed: 1, late: 4, maxBurst: 2,
                                        jitterUs: 15_000, capUs: 0, maxProcessingUs: 3_000)
        // Same bytes as `audio_report_round_trips_the_documented_layout` in daemon/src/audio.rs.
        let expected = Data([0, 0, 0, 7, 0, 0, 0x61, 0xA8, 0, 0, 0x4E, 0x20,
                             0, 20, 0, 3, 0, 2, 0, 1, 0, 4, 2,
                             0, 0, 0x3A, 0x98, 0, 0, 0, 0, 0, 0, 0x0B, 0xB8])
        precondition(report.encoded() == expected)
        precondition(expected.count == AudioPlayoutReport.byteCount)
    }

    static func waitsForTargetThenPlaysInOrder() {
        let buffer = AudioJitterBuffer()
        // Initial target: 20 ms estimate + 5 ms render quantum = 25 ms, so 3 packets.
        insert(buffer, 100, at: 0)
        insert(buffer, 101, at: 10)
        precondition(pull(buffer) == .silence)
        insert(buffer, 102, at: 20)
        expectDecode(buffer, 100)
        insert(buffer, 103, at: 30)
        expectDecode(buffer, 101)
        expectDecode(buffer, 102)
        precondition(buffer.takeReport(generation: 1)!.due == 3)
    }

    static func concealsHolesAndDropsLatePackets() {
        let buffer = AudioJitterBuffer()
        for seq in Int64(0)...2 { insert(buffer, seq, at: Double(seq) * 10) }
        expectDecode(buffer, 0)
        insert(buffer, 4, at: 40) // 3 missing
        expectDecode(buffer, 1)
        expectDecode(buffer, 2)
        precondition(pull(buffer) == .conceal)
        insert(buffer, 3, at: 45) // too late now
        expectDecode(buffer, 4)
        let report = buffer.takeReport(generation: 1)!
        precondition(report.due == 5 && report.primaryLost == 1 && report.concealed == 1)
        precondition(report.late == 1 && report.maxBurst == 1 && report.recovered == 0)
    }

    static func redundantCopiesFillHoles() {
        let buffer = AudioJitterBuffer()
        for seq in Int64(0)...2 { insert(buffer, seq, at: Double(seq) * 10) }
        expectDecode(buffer, 0)
        // Datagram 4 carries a copy of 3, whose own datagram was lost.
        insert(buffer, 4, at: 40)
        insert(buffer, 3, at: 40, primary: false)
        // A copy of an already-played packet is normal, not late.
        insert(buffer, 0, at: 40, primary: false)
        for seq in Int64(1)...4 { expectDecode(buffer, seq) }
        let report = buffer.takeReport(generation: 1)!
        precondition(report.primaryLost == 1 && report.recovered == 1 && report.concealed == 0)
        precondition(report.late == 0)
    }

    static func underrunConcealsBrieflyThenRefills() {
        let buffer = AudioJitterBuffer()
        for seq in Int64(0)...2 { insert(buffer, seq, at: Double(seq) * 10) }
        for seq in Int64(0)...2 { expectDecode(buffer, seq) }
        for _ in 0..<AudioJitterBuffer.maxConcealedRun {
            precondition(pull(buffer) == .conceal)
        }
        precondition(pull(buffer) == .silence)
        precondition(!buffer.isPlaying)
        // Back, but refills to the target before playing.
        insert(buffer, 10, at: 100)
        precondition(pull(buffer) == .silence)
        insert(buffer, 11, at: 110)
        insert(buffer, 12, at: 120)
        expectDecode(buffer, 10)
    }

    static func mergesWhenFarTooDeep() {
        let buffer = AudioJitterBuffer()
        for seq in Int64(0)...2 { insert(buffer, seq, at: Double(seq) * 10) }
        expectDecode(buffer, 0)
        // A 300 ms burst arrives at once.
        for seq in Int64(3)...32 { insert(buffer, seq, at: 30) }
        var merged = false
        for _ in 0..<100 {
            if case .merge(let first, let second) = pull(buffer) {
                precondition(first == 1 && second == 1)
                merged = true
                break
            }
        }
        precondition(merged)
    }

    /// Playback that starts while packets are delayed (a link still busy with the first
    /// keyframes) leaves a standing excess once the delay clears. It must drain by merging, not
    /// ride on the daemon's slow clock pace.
    static func drainsModerateExcessQuickly() {
        let buffer = AudioJitterBuffer()
        // Packet k is sent at 10k ms; its delay shrinks from 40 ms to 0 over the first second.
        func arrival(_ k: Int64) -> Double {
            let sent = Double(k) * 10
            return sent + max(0, 40 - sent / 25)
        }
        var seq: Int64 = 0
        var now = 0.0
        var merges = 0
        var peakDepth = 0.0
        for _ in 0..<2000 {
            while arrival(seq) <= now {
                insert(buffer, seq, at: arrival(seq))
                seq += 1
            }
            if case .merge = pull(buffer) { merges += 1 }
            peakDepth = max(peakDepth, buffer.smoothedDepthMs)
            now += 10
        }
        let band = max(AudioJitterBuffer.adjustBandMs, AudioJitterBuffer.adjustBandFraction * buffer.targetMs)
        precondition(peakDepth > 60, "peak \(peakDepth)")
        precondition(merges >= 2, "merges \(merges)")
        precondition(buffer.smoothedDepthMs <= buffer.targetMs + band + 5,
                     "depth \(buffer.smoothedDepthMs) target \(buffer.targetMs)")
    }

    static func newStreamForgetsTheTarget() {
        let buffer = AudioJitterBuffer()
        var now = 0.0
        for i in Int64(0)..<200 {
            insert(buffer, i, at: now + (i % 10 == 0 ? 150 : 0))
            now += 10
        }
        let learned = buffer.targetMs
        precondition(learned > 100, "target \(learned)")
        buffer.reset()
        precondition(buffer.targetMs == learned, "a resync keeps the target")
        buffer.reset(newStream: true)
        precondition(buffer.targetMs == 25, "target \(buffer.targetMs)")
    }

    static func resyncsAfterAJump() {
        let buffer = AudioJitterBuffer()
        for seq in Int64(0)...2 { insert(buffer, seq, at: Double(seq) * 10) }
        expectDecode(buffer, 0)
        // A far-off redundant copy alone doesn't resync.
        insert(buffer, 5000, at: 30, primary: false)
        expectDecode(buffer, 1)
        for seq in Int64(5000)...5002 { insert(buffer, seq, at: 40) }
        expectDecode(buffer, 5000)
    }

    static func targetFollowsJitter() {
        let buffer = AudioJitterBuffer()
        var now = 0.0
        var seq: Int64 = 0
        // Steady for 16 s: falls from the initial estimate at 2 ms/s and settles at
        // 10 ms + 5 ms quantum = the 15 ms floor.
        for _ in 0..<1600 {
            insert(buffer, seq, at: now)
            seq += 1
            now += 10
        }
        precondition(buffer.targetMs == 15, "target \(buffer.targetMs)")
        // Every 20th packet 60 ms late (5%): the target covers it.
        for i in 0..<200 {
            insert(buffer, seq, at: now + (i % 20 == 0 ? 60 : 0))
            seq += 1
            now += 10
        }
        precondition(buffer.targetMs >= 70, "target \(buffer.targetMs)")
    }

    static func targetRespectsVideoCap() {
        let buffer = AudioJitterBuffer()
        buffer.maxTargetMs = 40
        var now = 0.0
        for i in Int64(0)..<200 {
            insert(buffer, i, at: now + (i % 10 == 0 ? 150 : 0))
            now += 10
        }
        precondition(buffer.targetMs == 40, "target \(buffer.targetMs)")
        buffer.maxTargetMs = 0
        precondition(buffer.targetMs == AudioJitterBuffer.floorMs)
    }
}

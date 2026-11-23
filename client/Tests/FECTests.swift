import Foundation

// clang -O2 -c Screx/GF256Kernel.c -o /tmp/gf256-kernel.o
// swiftc -O -import-objc-header Screx/Screx-Bridging-Header.h Screx/FEC.swift \
//   Tests/FECTests.swift /tmp/gf256-kernel.o -o /tmp/fec-tests && /tmp/fec-tests
@main
struct FECTests {
    private static func gfMulRef(_ aIn: UInt8, _ bIn: UInt8) -> UInt8 {
        var a = aIn
        var b = bIn
        var res: UInt8 = 0
        for _ in 0..<8 {
            if (b & 1) != 0 { res ^= a }
            let hi = a & 0x80
            a &<<= 1
            if hi != 0 { a ^= 0x1D }
            b >>= 1
        }
        return res
    }

    private static func makeParity(
        dataShards: [Data],
        parityCount: Int
    ) -> [Data] {
        // Encode parity shards using the same Vandermonde systematic matrix by
        // recovering parity shards? Even simpler: compute parity via GF(2^8)
        // Vandermonde systematic encoding directly, matching reed-solomon-erasure.
        let k = dataShards.count
        let total = k + parityCount
        let shardSize = dataShards[0].count

        // Build Vandermonde matrix (total x k)
        var vand = [UInt8](repeating: 0, count: total * k)
        for r in 0..<total {
            var val: UInt8 = 1
            let x = UInt8(r)
            for c in 0..<k {
                vand[r * k + c] = val
                val = gfMulRef(val, x)
            }
        }
        // Invert top k x k
        var aug = [UInt8](repeating: 0, count: k * 2 * k)
        for i in 0..<k {
            for j in 0..<k { aug[i * 2 * k + j] = vand[i * k + j] }
            aug[i * 2 * k + k + i] = 1
        }
        for col in 0..<k {
            var pivot = col
            while aug[pivot * 2 * k + col] == 0 { pivot += 1 }
            if pivot != col {
                for j in 0..<(2 * k) {
                    let a = pivot * 2 * k + j
                    let b = col * 2 * k + j
                    aug.swapAt(a, b)
                }
            }
            // Inverse via Fermat: a^254
            var p = aug[col * 2 * k + col]
            var inv: UInt8 = 1
            var exp = 254
            while exp > 0 {
                if (exp & 1) != 0 { inv = gfMulRef(inv, p) }
                p = gfMulRef(p, p)
                exp >>= 1
            }
            for j in 0..<(2 * k) {
                aug[col * 2 * k + j] = gfMulRef(aug[col * 2 * k + j], inv)
            }
            for row in 0..<k where row != col {
                let f = aug[row * 2 * k + col]
                if f == 0 { continue }
                for j in 0..<(2 * k) {
                    aug[row * 2 * k + j] ^= gfMulRef(f, aug[col * 2 * k + j])
                }
            }
        }

        // Compute parity rows (k ..< total) = vand[r] * topInv
        var parities: [Data] = []
        for r in k..<total {
            var rowCoeff = [UInt8](repeating: 0, count: k)
            for c in 0..<k {
                var s: UInt8 = 0
                for m in 0..<k {
                    s ^= gfMulRef(vand[r * k + m], aug[m * 2 * k + k + c])
                }
                rowCoeff[c] = s
            }
            var out = Data(repeating: 0, count: shardSize)
            out.withUnsafeMutableBytes { dstRaw in
                let dst = dstRaw.bindMemory(to: UInt8.self).baseAddress!
                for c in 0..<k {
                    dataShards[c].withUnsafeBytes { srcRaw in
                        let src = srcRaw.bindMemory(to: UInt8.self).baseAddress!
                        screx_gf256_mul_add(dst, src, shardSize, rowCoeff[c])
                    }
                }
            }
            parities.append(out)
        }
        return parities
    }

    static func main() {
        // 1. Exhaustive GF(2^8) kernel verification across all 256*256 pairs
        var src = [UInt8](0...255)
        for coeff in 0...255 {
            var dst = [UInt8](repeating: 0x5A, count: 256)
            var scale = src
            screx_gf256_mul_add(&dst, &src, 256, UInt8(coeff))
            screx_gf256_mul_scale(&scale, 256, UInt8(coeff))
            for i in 0..<256 {
                let expected = gfMulRef(UInt8(i), UInt8(coeff))
                precondition(dst[i] == (0x5A ^ expected), "mul_add mismatch at coeff=\(coeff) x=\(i)")
                precondition(scale[i] == expected, "mul_scale mismatch at coeff=\(coeff) x=\(i)")
            }
        }

        // 2. Odd/unaligned lengths (0..65 + 1350)
        for len in Array(0...65) + [1350] {
            var s = (0..<len).map { UInt8(($0 * 37 + 13) & 0xFF) }
            var d = (0..<len).map { UInt8(($0 * 11 + 7) & 0xFF) }
            var expected = d
            for i in 0..<len { expected[i] ^= gfMulRef(s[i], 0xB7) }
            screx_gf256_mul_add(&d, &s, len, 0xB7)
            precondition(d == expected, "mul_add tail mismatch at len=\(len)")
        }

        // 3. End-to-end Reed-Solomon recovery across multiple block shapes & loss patterns
        for (dataCount, parityCount, shardSize, dropped) in [
            (10, 4, 1350, [0, 3, 7, 9]),
            (32, 8, 1337, [0, 1, 15, 16, 17, 30, 31]),
            (120, 12, 1350, [0, 5, 19, 44, 60, 77, 88, 100, 111, 119]),
        ] {
            let originals: [Data] = (0..<dataCount).map { shardIdx in
                var bytes = [UInt8](repeating: 0, count: shardSize)
                for b in 0..<shardSize {
                    bytes[b] = UInt8((shardIdx * 73 + b * 19 + 5) & 0xFF)
                }
                return Data(bytes)
            }
            let parities = makeParity(dataShards: originals, parityCount: parityCount)
            var shards: [Data?] = originals.map { Optional($0) } + parities.map { Optional($0) }
            for idx in dropped { shards[idx] = nil }

            let t0 = CFAbsoluteTimeGetCurrent()
            let ok = ReedSolomonDecoder.recover(shards: &shards, dataCount: dataCount, parityCount: parityCount)
            let dtMs = (CFAbsoluteTimeGetCurrent() - t0) * 1000.0
            precondition(ok, "recovery failed for (\(dataCount), \(parityCount))")
            for i in 0..<dataCount {
                precondition(shards[i] == originals[i], "recovered shard \(i) mismatch")
            }
            print(String(format: "FEC (%d+%d, %dB, %d lost): OK in %.3f ms", dataCount, parityCount, shardSize, dropped.count, dtMs))
        }

        print("All FEC NEON/flat-matrix tests passed")
    }
}

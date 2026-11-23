import Foundation

/// Minimal Reed-Solomon erasure decoder over GF(2^8) using the Vandermonde approach,
/// compatible with the `reed-solomon-erasure` Rust crate (which uses the same field).
enum ReedSolomonDecoder {

    /// Recover missing shards in-place.
    /// `shards[i]` is nil if that shard was lost. All present shards must be the same length.
    /// Returns true if recovery succeeded.
    static func recover(shards: inout [Data?], dataCount: Int, parityCount: Int) -> Bool {
        let totalCount = dataCount + parityCount
        guard shards.count == totalCount else { return false }

        // Find the shard size from any present shard
        guard let shardSize = shards.compactMap({ $0 }).first?.count else {
            return false
        }

        // Identify present and missing shard indices
        var presentIndices: [Int] = []
        var missingIndices: [Int] = []
        for i in 0..<totalCount {
            if shards[i] != nil {
                presentIndices.append(i)
            } else {
                missingIndices.append(i)
            }
        }

        // Need at least dataCount shards to recover
        guard presentIndices.count >= dataCount else { return false }
        if missingIndices.isEmpty { return true }

        // Use first dataCount present shards for the decoding matrix
        let usedIndices = Array(presentIndices.prefix(dataCount))

        // Build the sub-matrix from the Vandermonde-like coding matrix.
        // Stored flat row-major: element (r, c) is at `r * dataCount + c`.
        let codingMatrix = getCodingMatrix(dataCount: dataCount, parityCount: parityCount)

        // Look up or compute the inverse of the sub-matrix.
        let cacheKey = InvertMatrixKey(
            dataCount: dataCount,
            parityCount: parityCount,
            usedIndices: usedIndices
        )
        let invMatrix: [UInt8]
        if let cached = invertMatrixCache[cacheKey] {
            invMatrix = cached
        } else {
            var subMatrix = [UInt8](repeating: 0, count: dataCount * dataCount)
            codingMatrix.withUnsafeBufferPointer { srcBuf in
                subMatrix.withUnsafeMutableBufferPointer { dstBuf in
                    guard let srcBase = srcBuf.baseAddress,
                          let dstBase = dstBuf.baseAddress else { return }
                    for i in 0..<dataCount {
                        let srcRow = usedIndices[i] * dataCount
                        dstBase.advanced(by: i * dataCount)
                            .update(from: srcBase.advanced(by: srcRow), count: dataCount)
                    }
                }
            }
            guard let inverted = invertMatrix(subMatrix, n: dataCount) else { return false }
            invertMatrixCache[cacheKey] = inverted
            // The key space is small but unbounded under pathological loss;
            // reset if we ever grow too large. Recovery is bursty, so a reset
            // only costs a few re-inversions.
            if invertMatrixCache.count > invertMatrixCacheMaxEntries {
                invertMatrixCache.removeAll(keepingCapacity: true)
                invertMatrixCache[cacheKey] = inverted
            }
            invMatrix = inverted
        }

        // Collect the data from present shards (in the order of usedIndices)
        var presentData: [Data] = []
        presentData.reserveCapacity(dataCount)
        for idx in usedIndices {
            presentData.append(shards[idx]!)
        }

        // Recover missing data shards (indices < dataCount).
        // Callers only consume data shards, so skip reconstructing missing parity shards.
        for missingIdx in missingIndices where missingIdx < dataCount {
            var recovered = Data(repeating: 0, count: shardSize)
            let rowOffset = missingIdx * dataCount
            recovered.withUnsafeMutableBytes { dstRaw in
                guard let dstPtr = dstRaw.bindMemory(to: UInt8.self).baseAddress else { return }
                for j in 0..<dataCount {
                    let coeff = invMatrix[rowOffset + j]
                    if coeff == 0 { continue }
                    presentData[j].withUnsafeBytes { srcRaw in
                        guard let srcPtr = srcRaw.bindMemory(to: UInt8.self).baseAddress else { return }
                        let count = min(shardSize, srcRaw.count)
                        screx_gf256_mul_add(dstPtr, srcPtr, count, coeff)
                    }
                }
            }
            shards[missingIdx] = recovered
        }

        return true
    }

    // MARK: - GF(2^8) arithmetic with primitive polynomial 0x1D (same as reed-solomon-erasure)

    private static let gfExp: [UInt8] = buildExpTable()
    private static let gfLog: [UInt8] = buildLogTable()

    private static func buildExpTable() -> [UInt8] {
        var table = [UInt8](repeating: 0, count: 512)
        var val: UInt16 = 1
        for i in 0..<255 {
            table[i] = UInt8(val)
            table[i + 255] = UInt8(val)
            val <<= 1
            if val >= 256 {
                val ^= 0x11D // x^8 + x^4 + x^3 + x^2 + 1
            }
        }
        return table
    }

    private static func buildLogTable() -> [UInt8] {
        var table = [UInt8](repeating: 0, count: 256)
        for i in 0..<255 {
            table[Int(gfExp[i])] = UInt8(i)
        }
        return table
    }

    private static func gf_mul(_ a: UInt8, _ b: UInt8) -> UInt8 {
        if a == 0 || b == 0 { return 0 }
        return gfExp[Int(gfLog[Int(a)]) + Int(gfLog[Int(b)])]
    }

    private static func gf_inv(_ a: UInt8) -> UInt8 {
        if a == 0 { return 0 }
        return gfExp[255 - Int(gfLog[Int(a)])]
    }

    // MARK: - Coding matrix (Vandermonde / Cauchy, compatible with reed-solomon-erasure)

    /// Flat row-major coding matrices: `totalCount * dataCount` bytes.
    private static var matrixCache: [UInt64: [UInt8]] = [:]
    private static let matrixCacheMaxEntries = 64

    /// Cache key for an inverted decode sub-matrix.
    /// `usedIndices` is the ordered set of present data shards used to build the
    /// sub-matrix. The same loss pattern then reuses the inverse.
    private struct InvertMatrixKey: Hashable {
        let dataCount: Int
        let parityCount: Int
        let usedIndices: [Int]
    }

    /// Flat row-major inverted sub-matrices (`dataCount * dataCount` bytes) keyed
    /// by the loss pattern that produced them. Recovery is driven from a single
    /// NWConnection receive queue, so no lock is required; if that ever changes,
    /// wrap cache access in an NSLock.
    private static var invertMatrixCache: [InvertMatrixKey: [UInt8]] = [:]
    private static let invertMatrixCacheMaxEntries = 64

    private static func getCodingMatrix(dataCount: Int, parityCount: Int) -> [UInt8] {
        let key = UInt64(dataCount) << 32 | UInt64(parityCount)
        if let cached = matrixCache[key] { return cached }
        let matrix = buildCodingMatrix(dataCount: dataCount, parityCount: parityCount)
        if matrixCache.count >= matrixCacheMaxEntries {
            matrixCache.removeAll(keepingCapacity: true)
        }
        matrixCache[key] = matrix
        return matrix
    }

    private static func buildCodingMatrix(dataCount: Int, parityCount: Int) -> [UInt8] {
        let totalCount = dataCount + parityCount

        // Vandermonde-derived parity rows (same as reed-solomon-erasure):
        // build a Vandermonde matrix, then multiply by inverse of top square to get systematic form.
        var vand = [UInt8](repeating: 0, count: totalCount * dataCount)
        for r in 0..<totalCount {
            var val: UInt8 = 1
            let x = UInt8(r)
            let rowBase = r * dataCount
            for c in 0..<dataCount {
                vand[rowBase + c] = val
                val = gf_mul(val, x)
            }
        }

        // Invert top square (`dataCount * dataCount`).
        let topSquare = Array(vand[0..<(dataCount * dataCount)])
        guard let topInv = invertMatrix(topSquare, n: dataCount) else {
            return vand
        }

        // Multiply full Vandermonde by top-inverse to get systematic form.
        var matrix = [UInt8](repeating: 0, count: totalCount * dataCount)
        matrix.withUnsafeMutableBufferPointer { dstBuf in
            topInv.withUnsafeBufferPointer { invBuf in
                guard let dstBase = dstBuf.baseAddress,
                      let invBase = invBuf.baseAddress else { return }
                for r in 0..<totalCount {
                    let vandRow = r * dataCount
                    let dstRow = dstBase.advanced(by: vandRow)
                    for k in 0..<dataCount {
                        let coeff = vand[vandRow + k]
                        if coeff == 0 { continue }
                        screx_gf256_mul_add(dstRow, invBase.advanced(by: k * dataCount), dataCount, coeff)
                    }
                }
            }
        }

        return matrix
    }

    /// Invert a flat row-major `n x n` matrix over GF(2^8) using Gauss-Jordan elimination.
    private static func invertMatrix(_ m: [UInt8], n: Int) -> [UInt8]? {
        let stride = 2 * n
        var aug = [UInt8](repeating: 0, count: n * stride)
        aug.withUnsafeMutableBufferPointer { augBuf in
            m.withUnsafeBufferPointer { srcBuf in
                guard let augBase = augBuf.baseAddress,
                      let srcBase = srcBuf.baseAddress else { return }
                for i in 0..<n {
                    augBase.advanced(by: i * stride)
                        .update(from: srcBase.advanced(by: i * n), count: n)
                    augBase[i * stride + n + i] = 1
                }
            }
        }

        let ok = aug.withUnsafeMutableBufferPointer { augBuf -> Bool in
            guard let base = augBuf.baseAddress else { return false }
            for col in 0..<n {
                // Find pivot
                var pivotRow = -1
                for row in col..<n {
                    if base[row * stride + col] != 0 {
                        pivotRow = row
                        break
                    }
                }
                guard pivotRow >= 0 else { return false }

                if pivotRow != col {
                    let r1 = base.advanced(by: pivotRow * stride)
                    let r2 = base.advanced(by: col * stride)
                    for j in 0..<stride {
                        let tmp = r1[j]
                        r1[j] = r2[j]
                        r2[j] = tmp
                    }
                }

                // Scale pivot row so pivot becomes 1
                let pivotPtr = base.advanced(by: col * stride)
                let scale = gf_inv(pivotPtr[col])
                screx_gf256_mul_scale(pivotPtr, stride, scale)

                // Eliminate column in all other rows
                for row in 0..<n where row != col {
                    let rowPtr = base.advanced(by: row * stride)
                    let factor = rowPtr[col]
                    if factor == 0 { continue }
                    screx_gf256_mul_add(rowPtr, pivotPtr, stride, factor)
                }
            }
            return true
        }
        guard ok else { return nil }

        var result = [UInt8](repeating: 0, count: n * n)
        result.withUnsafeMutableBufferPointer { resBuf in
            aug.withUnsafeBufferPointer { augBuf in
                guard let resBase = resBuf.baseAddress,
                      let augBase = augBuf.baseAddress else { return }
                for i in 0..<n {
                    resBase.advanced(by: i * n)
                        .update(from: augBase.advanced(by: i * stride + n), count: n)
                }
            }
        }
        return result
    }
}

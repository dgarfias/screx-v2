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

        // Build the sub-matrix from the Vandermonde-like coding matrix
        // The coding matrix for reed-solomon-erasure uses an inverted Vandermonde * Vandermonde approach.
        // Row i of the coding matrix = row usedIndices[i] of the full matrix.
        // The full matrix: top dataCount rows = identity, bottom parityCount rows = Cauchy-like.
        let codingMatrix = getCodingMatrix(dataCount: dataCount, parityCount: parityCount)

        // Extract the sub-matrix for the used indices
        var subMatrix = [[UInt8]](repeating: [UInt8](repeating: 0, count: dataCount), count: dataCount)
        for i in 0..<dataCount {
            subMatrix[i] = codingMatrix[usedIndices[i]]
        }

        // Look up or compute the inverse of the sub-matrix.
        let cacheKey = InvertMatrixKey(
            dataCount: dataCount,
            parityCount: parityCount,
            usedIndices: usedIndices
        )
        let invMatrix: [[UInt8]]
        if let cached = invertMatrixCache[cacheKey] {
            invMatrix = cached
        } else {
            guard let inverted = invertMatrix(subMatrix) else { return false }
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
        for idx in usedIndices {
            presentData.append(shards[idx]!)
        }

        // Recover missing data shards (indices < dataCount)
        for missingIdx in missingIndices where missingIdx < dataCount {
            var recovered = Data(repeating: 0, count: shardSize)
            for j in 0..<dataCount {
                let coeff = invMatrix[missingIdx][j]
                if coeff == 0 { continue }
                gf_mul_add(&recovered, presentData[j], coeff)
            }
            shards[missingIdx] = recovered
        }

        // Recover missing parity shards (indices >= dataCount) — reconstruct from data
        for missingIdx in missingIndices where missingIdx >= dataCount {
            var recovered = Data(repeating: 0, count: shardSize)
            for j in 0..<dataCount {
                let coeff = codingMatrix[missingIdx][j]
                if coeff == 0 { continue }
                gf_mul_add(&recovered, shards[j]!, coeff)
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

    /// dst[i] ^= src[i] * coeff (for all i)
    private static func gf_mul_add(_ dst: inout Data, _ src: Data, _ coeff: UInt8) {
        if coeff == 0 { return }
        if coeff == 1 {
            dst.withUnsafeMutableBytes { dstRaw in
                src.withUnsafeBytes { srcRaw in
                    guard let dstPtr = dstRaw.bindMemory(to: UInt8.self).baseAddress,
                          let srcPtr = srcRaw.bindMemory(to: UInt8.self).baseAddress else { return }
                    let count = min(dstRaw.count, srcRaw.count)
                    for i in 0..<count {
                        dstPtr[i] ^= srcPtr[i]
                    }
                }
            }
            return
        }
        let logCoeff = Int(gfLog[Int(coeff)])
        dst.withUnsafeMutableBytes { dstRaw in
            src.withUnsafeBytes { srcRaw in
                guard let dstPtr = dstRaw.bindMemory(to: UInt8.self).baseAddress,
                      let srcPtr = srcRaw.bindMemory(to: UInt8.self).baseAddress else { return }
                let count = min(dstRaw.count, srcRaw.count)
                for i in 0..<count {
                    let v = srcPtr[i]
                    if v != 0 {
                        dstPtr[i] ^= gfExp[Int(gfLog[Int(v)]) + logCoeff]
                    }
                }
            }
        }
    }

    // MARK: - Coding matrix (Vandermonde / Cauchy, compatible with reed-solomon-erasure)

    private static var matrixCache: [UInt64: [[UInt8]]] = [:]

    /// Cache key for an inverted decode sub-matrix.
    /// `usedIndices` is the ordered set of present data shards used to build the
    /// sub-matrix. The same loss pattern then reuses the inverse.
    private struct InvertMatrixKey: Hashable {
        let dataCount: Int
        let parityCount: Int
        let usedIndices: [Int]
    }

    /// Inverted sub-matrices keyed by the loss pattern that produced them.
    /// Recovery is driven from a single NWConnection receive queue, so no lock
    /// is required; if that ever changes, wrap cache access in an NSLock.
    private static var invertMatrixCache: [InvertMatrixKey: [[UInt8]]] = [:]
    private static let invertMatrixCacheMaxEntries = 64

    private static func getCodingMatrix(dataCount: Int, parityCount: Int) -> [[UInt8]] {
        let key = UInt64(dataCount) << 32 | UInt64(parityCount)
        if let cached = matrixCache[key] { return cached }
        let matrix = buildCodingMatrix(dataCount: dataCount, parityCount: parityCount)
        matrixCache[key] = matrix
        return matrix
    }

    private static func buildCodingMatrix(dataCount: Int, parityCount: Int) -> [[UInt8]] {
        let totalCount = dataCount + parityCount
        var matrix = [[UInt8]](repeating: [UInt8](repeating: 0, count: dataCount), count: totalCount)

        // Identity for data shards
        for i in 0..<dataCount {
            matrix[i][i] = 1
        }

        // Vandermonde-derived parity rows (same as reed-solomon-erasure)
        // Build a Vandermonde matrix, then multiply by inverse of top square to get systematic form
        var vand = [[UInt8]](repeating: [UInt8](repeating: 0, count: dataCount), count: totalCount)
        for r in 0..<totalCount {
            var val: UInt8 = 1
            let x = UInt8(r)
            for c in 0..<dataCount {
                vand[r][c] = val
                val = gf_mul(val, x)
            }
        }

        // Invert top square
        let topSquare = Array(vand.prefix(dataCount))
        guard let topInv = invertMatrix(topSquare) else {
            // Fallback to simple Vandermonde without systematic form
            return vand
        }

        // Multiply full Vandermonde by top-inverse to get systematic form
        for r in 0..<totalCount {
            for c in 0..<dataCount {
                var sum: UInt8 = 0
                for k in 0..<dataCount {
                    sum ^= gf_mul(vand[r][k], topInv[k][c])
                }
                matrix[r][c] = sum
            }
        }

        return matrix
    }

    /// Invert a square matrix over GF(2^8) using Gauss-Jordan elimination
    private static func invertMatrix(_ m: [[UInt8]]) -> [[UInt8]]? {
        let n = m.count
        // Augmented matrix [m | I]
        var aug = [[UInt8]](repeating: [UInt8](repeating: 0, count: 2 * n), count: n)
        for i in 0..<n {
            for j in 0..<n {
                aug[i][j] = m[i][j]
            }
            aug[i][n + i] = 1
        }

        for col in 0..<n {
            // Find pivot
            var pivotRow = -1
            for row in col..<n {
                if aug[row][col] != 0 {
                    pivotRow = row
                    break
                }
            }
            guard pivotRow >= 0 else { return nil }

            if pivotRow != col {
                aug.swapAt(pivotRow, col)
            }

            // Scale pivot row
            let scale = gf_inv(aug[col][col])
            for j in 0..<(2 * n) {
                aug[col][j] = gf_mul(aug[col][j], scale)
            }

            // Eliminate column
            for row in 0..<n where row != col {
                let factor = aug[row][col]
                if factor == 0 { continue }
                for j in 0..<(2 * n) {
                    aug[row][j] ^= gf_mul(factor, aug[col][j])
                }
            }
        }

        // Extract inverse
        var result = [[UInt8]](repeating: [UInt8](repeating: 0, count: n), count: n)
        for i in 0..<n {
            result[i] = Array(aug[i][n..<(2 * n)])
        }
        return result
    }
}

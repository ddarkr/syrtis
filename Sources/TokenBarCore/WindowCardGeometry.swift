import Foundation

/// Pure geometry for the in-window usage card. Everything the card draws is
/// derived here so it can be asserted without a view: the SwiftUI layer only
/// strokes and fills what these functions return.
///
/// The load-bearing property is that `bars` and `hits` never see `metric`.
/// A flow has no "remaining" version, so flipping used/remaining must leave
/// the usage geometry bit-identical (`V12`).

public enum QuotaMetric: String, Sendable, Equatable {
    case used
    case remaining

    /// The provider always reports used%; remaining is the complement.
    public func value(fromUsedPercent used: Double) -> Double {
        self == .used ? used : 100 - used
    }
}

public struct QuotaSample: Equatable, Sendable {
    public let atMs: Int64
    public let usedPercent: Double

    public init(atMs: Int64, usedPercent: Double) {
        self.atMs = atMs
        self.usedPercent = usedPercent
    }
}

/// x and width are fractions of the window; height is a fraction of the tallest
/// bar. Nothing here is in points — the view owns pixels.
public struct BarRect: Equatable, Sendable {
    public let x: Double
    public let width: Double
    public let height: Double
    /// True when ex-cache-read weight is zero, including cost-only and
    /// cache-read-only usage. Its compact hover counters remain available.
    public let isEmpty: Bool
}

public struct CurvePoint: Equatable, Sendable {
    public let x: Double
    /// Already through `QuotaMetric.value`, so 0...100 in the displayed sense.
    public let y: Double

    public init(x: Double, y: Double) {
        self.x = x
        self.y = y
    }
}

/// One hit target. Zones tile `[windowStart, now]` exactly — see `zones(...)`.
public struct HitZone: Equatable, Sendable {
    public let index: Int
    public let loMs: Int64
    public let hiMs: Int64
    public let x: Double
    public let width: Double
    /// Absent for the region before the first sample and after the last one:
    /// usage happened, but no quota reading closes the interval.
    public let closingSample: QuotaSample?
    /// The reading the interval opened on. Present with `closingSample` it
    /// gives the quota this interval actually consumed — the number the bars
    /// are supposed to explain.
    public let openingSample: QuotaSample?

    /// How much quota this interval consumed, in the direction the card is
    /// currently read: positive when counting up, negative when counting down.
    /// Nil when either end has no reading, because then nothing was measured.
    public func consumed(_ metric: QuotaMetric) -> Double? {
        guard let a = openingSample, let b = closingSample else { return nil }
        return metric.value(fromUsedPercent: b.usedPercent)
            - metric.value(fromUsedPercent: a.usedPercent)
    }
}

public struct ChartGeometry: Equatable, Sendable {
    /// Where `now` falls in the drawn window, 0...1. Everything to its right
    /// has not happened: no bars, no line, and no hit zones.
    public let nowX: Double
    /// Where the first quota sample falls, or `nowX` when there is none.
    /// Left of it the line is not drawn — the app was not running to sample.
    public let firstSampleX: Double
    public let bars: [BarRect]
    public let hits: [HitZone]
    /// Sample positions, drawn as dots. The curve between them is interpolation.
    public let samplePoints: [CurvePoint]
    /// The interpolated polyline. Empty when fewer than two samples fall inside.
    public let curve: [CurvePoint]

    public init(
        nowX: Double, firstSampleX: Double, bars: [BarRect], hits: [HitZone],
        samplePoints: [CurvePoint], curve: [CurvePoint]
    ) {
        self.nowX = nowX
        self.firstSampleX = firstSampleX
        self.bars = bars
        self.hits = hits
        self.samplePoints = samplePoints
        self.curve = curve
    }
}

public enum WindowCardGeometry {
    /// How many interpolated points per segment. Purely visual smoothness; the
    /// shape is fixed by `monotoneCurve` regardless of this value.
    public static let curveResolution = 8

    // MARK: - Zones

    /// Boundaries are `{windowStart} ∪ {sample times} ∪ {now}`, deduplicated and
    /// clamped, so the zones tile `[windowStart, now]` with no gap, no overlap,
    /// and **no minimum width** (`V13`).
    ///
    /// A minimum width is what pushes the last zone past `now`, and the future
    /// must be unhittable by construction rather than by intent. A zone too
    /// narrow to hit means too many buckets, not a wider zone.
    /// `windowEndMs` only sets the horizontal scale: zones still stop at `now`.
    public static func zones(
        windowStartMs: Int64, windowEndMs: Int64, nowMs: Int64,
        samples: [QuotaSample]
    ) -> [HitZone] {
        guard nowMs > windowStartMs, windowEndMs > windowStartMs else { return [] }
        var edges: [Int64] = [windowStartMs]
        for s in samples where s.atMs > windowStartMs && s.atMs < nowMs {
            if s.atMs != edges.last { edges.append(s.atMs) }
        }
        edges.append(nowMs)

        // Fractions are of the whole window, not of the elapsed part, so the
        // future region keeps its share of the width.
        let span = Double(windowEndMs - windowStartMs)
        var out: [HitZone] = []
        for i in 0..<(edges.count - 1) {
            let lo = edges[i], hi = edges[i + 1]
            out.append(HitZone(
                index: i, loMs: lo, hiMs: hi,
                x: Double(lo - windowStartMs) / span,
                width: Double(hi - lo) / span,
                closingSample: samples.first { $0.atMs == hi },
                openingSample: samples.first { $0.atMs == lo }))
        }
        return out
    }

    // MARK: - Geometry

    /// The metric-free half: bars and hit zones. Native counters already
    /// intersect each zone with the half-open current interval.
    /// Compute once per data load, not per body evaluation.
    public static func usageGeometry(
        windowStartMs: Int64, windowEndMs: Int64, nowMs: Int64,
        samples: [QuotaSample], zoneCounters: [WindowUsageBatch.Zone]
    ) -> (bars: [BarRect], hits: [HitZone]) {
        let hits = zones(windowStartMs: windowStartMs, windowEndMs: windowEndMs,
                         nowMs: nowMs, samples: samples)
        // Bars are sized by every token class except cache read. Cache read is
        // 200x the volume at a tenth the price, so including it decouples the
        // bars from the line; cache WRITE costs 1.25x base input and must stay.
        // Measured discrimination (moved vs still buckets): 4.5x this way,
        // 1.2x with input+output+reasoning alone.
        //
        let byIndex = Dictionary(
            zoneCounters.map { ($0.index, $0.tokensExCacheRead) },
            uniquingKeysWith: { first, _ in first })
        let weights = hits.map { byIndex[Int64($0.index)] ?? 0 }
        let tallest = max(weights.max() ?? 0, 1)
        let bars = zip(hits, weights).map { zone, w in
            BarRect(x: zone.x, width: zone.width,
                    height: Double(w) / Double(tallest), isEmpty: w == 0)
        }
        return (bars, hits)
    }

    /// The metric-dependent half: cheap, O(samples), so it may run per body.
    public static func quotaGeometry(
        windowStartMs: Int64, windowEndMs: Int64, nowMs: Int64,
        samples: [QuotaSample], metric: QuotaMetric
    ) -> (samplePoints: [CurvePoint], curve: [CurvePoint], nowX: Double, firstSampleX: Double) {
        let span = Double(max(windowEndMs - windowStartMs, 1))
        let inside = samples.filter { $0.atMs >= windowStartMs && $0.atMs <= nowMs }
        let points = inside.map {
            CurvePoint(x: Double($0.atMs - windowStartMs) / span,
                       y: metric.value(fromUsedPercent: $0.usedPercent))
        }
        let nowX = Double(nowMs - windowStartMs) / span
        return (points, monotoneCurve(points), nowX, points.first?.x ?? nowX)
    }

    /// `metric` reaches the curve and nothing else.
    public static func chartGeometry(
        windowStartMs: Int64, windowEndMs: Int64, nowMs: Int64,
        samples: [QuotaSample], zoneCounters: [WindowUsageBatch.Zone],
        metric: QuotaMetric
    ) -> ChartGeometry {
        let usage = usageGeometry(
            windowStartMs: windowStartMs, windowEndMs: windowEndMs, nowMs: nowMs,
            samples: samples, zoneCounters: zoneCounters)
        let quota = quotaGeometry(
            windowStartMs: windowStartMs, windowEndMs: windowEndMs, nowMs: nowMs,
            samples: samples, metric: metric)
        return ChartGeometry(
            nowX: quota.nowX, firstSampleX: quota.firstSampleX,
            bars: usage.bars, hits: usage.hits,
            samplePoints: quota.samplePoints, curve: quota.curve)
    }

    // MARK: - Interpolation

    /// Fritsch-Carlson monotone cubic, sampled into a polyline.
    ///
    /// Not Catmull-Rom: quota is monotone, and an overshoot between two rising
    /// samples draws a refill that never happened. The `α²+β²>9` clamp is what
    /// guarantees every interpolated point stays between its two endpoints
    /// (`V14`) — it is not a smoothing nicety.
    public static func monotoneCurve(_ p: [CurvePoint]) -> [CurvePoint] {
        let n = p.count
        guard n >= 2 else { return [] }

        var dx = [Double](), d = [Double]()
        for i in 0..<(n - 1) {
            let h = p[i + 1].x - p[i].x
            dx.append(h)
            d.append(h == 0 ? 0 : (p[i + 1].y - p[i].y) / h)
        }
        var m = [d[0]]
        for i in 1..<(n - 1) {
            // Zero at a turn. Averaging adjacent secants unconditionally leaves
            // a nonzero tangent at a local extremum, and the `α²+β²>9` clamp
            // below bounds a tangent's MAGNITUDE without touching its sign — so
            // a provider correction that reverses direction (80 → 90 → 89)
            // interpolated above 90 between the last two points and drew quota
            // levels nobody observed. This is the sign half of the monotone
            // condition; the clamp is the magnitude half, and only one of them
            // was here.
            m.append(d[i - 1] * d[i] <= 0 ? 0 : (d[i - 1] + d[i]) / 2)
        }
        m.append(d[n - 2])
        for i in 0..<(n - 1) {
            if d[i] == 0 { m[i] = 0; m[i + 1] = 0; continue }
            let a = m[i] / d[i], b = m[i + 1] / d[i]
            let s = a * a + b * b
            if s > 9 {
                let t = 3 / s.squareRoot()
                m[i] = t * a * d[i]
                m[i + 1] = t * b * d[i]
            }
        }

        var out = [p[0]]
        for i in 0..<(n - 1) {
            let h = dx[i]
            for step in 1...curveResolution {
                let t = Double(step) / Double(curveResolution)
                let t2 = t * t, t3 = t2 * t
                // Cubic Hermite basis.
                let y = (2 * t3 - 3 * t2 + 1) * p[i].y
                    + (t3 - 2 * t2 + t) * h * m[i]
                    + (-2 * t3 + 3 * t2) * p[i + 1].y
                    + (t3 - t2) * h * m[i + 1]
                out.append(CurvePoint(x: p[i].x + t * h, y: y))
            }
        }
        return out
    }
}

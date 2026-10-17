import Foundation

// Read-only quota curve snapshot (`tb_quota_curve`). Keys match the Rust serde
// camelCase serialization exactly.
//
// The decoding here is deliberately stricter than the JSON shape. This payload
// crosses the C ABI from a producer that already refuses to emit anything but a
// validated series, so every property below is something Rust guarantees rather
// than something this layer hopes for. Accepting a value Rust cannot produce
// would let an ABI drift render as a plausible curve instead of failing closed,
// which is the same contract the pace enums in `AgentUsage.swift` already hold.

/// Where a point's window length came from. Same Rust `DurationSource` wire
/// values as `UsagePaceDurationSource`; kept as a separate type only because the
/// two payloads are decoded independently.
public enum QuotaCurveDurationSource: String, Decodable, Sendable, Equatable {
    case provider
    case contract
    case observed
}

/// Which writer recorded the sample. `importedV2` is the one-time schema-2
/// migration lane; everything recorded by the v3 writer is `liveV3`.
public enum QuotaCurveSampleOrigin: String, Decodable, Sendable, Equatable {
    case liveV3
    case importedV2
}

/// One admitted quota reading. `sampledAt` is the real observation time, never
/// a grid position: phase-bucket admission decides *whether* a reading is kept,
/// not when it was taken, so the points are unevenly spaced by construction.
public struct QuotaCurvePoint: Decodable, Sendable, Equatable {
    public let sampledAt: Int64
    public let usedPercent: Double
    public let resetAt: Int64
    /// Per point, not per cycle: one reset group can legitimately carry
    /// readings recorded under different window lengths and sources.
    public let durationSeconds: Int64
    public let durationSource: QuotaCurveDurationSource
    public let origin: QuotaCurveSampleOrigin
    /// Whether this point belongs to the cycle still running — answered by the
    /// producer, never re-derived here.
    ///
    /// The fold used to decide this by comparing `resetAt` against the curve's
    /// `activeResetAt`. Those are not comparable: `activeResetAt` is the RAW
    /// provider value and every stored `resetAt` has been through
    /// `normalize_sample_reset`, so the comparison silently failed whenever the
    /// provider's reset was not already on the quantum — the ordinary case, not
    /// the exotic one. The running cycle was then listed under "past windows",
    /// stood beside completed spans as though comparable, and counted toward
    /// the three-cycle equivalence threshold while still filling.
    ///
    /// Reproducing the quantization on this side would put the same rule in two
    /// languages, which is the arrangement that produced the bug. The engine
    /// knows which group is active; it now says so.
    public let isActiveGroup: Bool

    private enum CodingKeys: String, CodingKey {
        case sampledAt, usedPercent, resetAt, durationSeconds, durationSource, origin
        case isActiveGroup
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        sampledAt = try container.decode(Int64.self, forKey: .sampledAt)
        usedPercent = try container.decode(Double.self, forKey: .usedPercent)
        resetAt = try container.decode(Int64.self, forKey: .resetAt)
        durationSeconds = try container.decode(Int64.self, forKey: .durationSeconds)
        durationSource = try container.decode(QuotaCurveDurationSource.self, forKey: .durationSource)
        origin = try container.decode(QuotaCurveSampleOrigin.self, forKey: .origin)
        // Required, for the reason `activeResetAt` is: Rust always emits it, so
        // an absent key is ABI drift. Defaulting it to false would render every
        // point as finished history — the exact misreading this field exists to
        // remove, arriving silently instead of as a decode failure.
        guard container.contains(.isActiveGroup) else {
            throw QuotaCurve.corrupted(
                decoder, "the curve point is missing the isActiveGroup key")
        }
        isActiveGroup = try container.decode(Bool.self, forKey: .isActiveGroup)

        // Exactly `validate_sample` in agent_quota_history.rs: a sample that
        // fails any of these never enters the store, so seeing one here means
        // the payload did not come from that store.
        guard QuotaCurve.validDurationSeconds.contains(durationSeconds) else {
            throw QuotaCurve.corrupted(
                decoder, "durationSeconds falls outside the storable window length")
        }
        // `>= 0`, matching the store's widened admission: a fresh window reads
        // 0% used, and rejecting that meant no cycle recorded its own start.
        // This guard exists to detect a payload that did not come from the
        // store, so it has to move with the store or it would reject the very
        // samples the store now writes.
        guard usedPercent.isFinite, usedPercent >= 0, usedPercent <= 100 else {
            throw QuotaCurve.corrupted(decoder, "usedPercent must fall in [0, 100]")
        }
        let cycleStart = resetAt.subtractingReportingOverflow(durationSeconds)
        guard !cycleStart.overflow, (cycleStart.partialValue...resetAt).contains(sampledAt) else {
            throw QuotaCurve.corrupted(decoder, "sampledAt falls outside its own cycle")
        }
    }
}

/// What the snapshot covers. Deliberately not a completeness claim — whether a
/// requested range is fully covered is decided when drawing, against the
/// series' actual window length and the samples actually held.
public struct QuotaCurveCoverage: Decodable, Sendable, Equatable {
    public let oldestSampledAt: Int64
    public let newestSampledAt: Int64
    public let sampleCount: Int
}

public struct QuotaCurve: Decodable, Sendable, Equatable {
    public let points: [QuotaCurvePoint]
    public let coverage: QuotaCurveCoverage
    public let activeResetAt: Int64?
    /// The publication generation whose binding resolved this series' identity.
    /// It is not a data cutoff: the samples are the history as of the read.
    public let generation: UInt64

    private enum CodingKeys: String, CodingKey {
        case points, coverage, activeResetAt, generation
    }

    private struct SampleKey: Hashable {
        let sampledAt: Int64
        let resetAt: Int64
        let durationSeconds: Int64
    }

    /// Mirrors `valid_duration` in `agent_quota_duration.rs`, whose bound is
    /// `1...MAX_DURATION_SECONDS` (400 days). Both ends matter: a sample outside
    /// either one is refused by the writer, so a payload carrying it did not come
    /// from the store this call reads. `quota_curve_duration_bound_matches_swift`
    /// on the Rust side fails if that constant moves without this one.
    public static let validDurationSeconds: ClosedRange<Int64> = 1...(400 * 86_400)

    /// Mirrors `MAX_SAMPLES` in `agent_quota_history.rs`: one series is never
    /// stored holding more than this, so a longer curve did not come from that
    /// store. `quota_curve_sample_ceiling_matches_swift` fails if it moves.
    public static let maxPoints = 65_536

    static func corrupted(_ decoder: Decoder, _ description: String) -> DecodingError {
        DecodingError.dataCorrupted(.init(
            codingPath: decoder.codingPath, debugDescription: description))
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        points = try container.decode([QuotaCurvePoint].self, forKey: .points)
        coverage = try container.decode(QuotaCurveCoverage.self, forKey: .coverage)
        // Same distinction the envelope's `data` key needed: Rust always emits
        // this field (`Option<i64>` with no `skip_serializing_if`), so an absent
        // key is ABI drift while an explicit null is the real answer for a
        // series with no active cycle. `decodeIfPresent` alone maps both to nil,
        // which would let drift render as "no active cycle" — and a current
        // incomplete group would then be drawn as inactive.
        guard container.contains(.activeResetAt) else {
            throw Self.corrupted(decoder, "the curve is missing the activeResetAt key")
        }
        activeResetAt = try container.decodeIfPresent(Int64.self, forKey: .activeResetAt)
        generation = try container.decode(UInt64.self, forKey: .generation)

        // Absent history is `null`, never an empty curve: the producer returns
        // early when the series holds no samples, and `quota_curve_payload`
        // errors rather than emitting a coverage window it cannot derive.
        guard let first = points.first, let last = points.last else {
            throw Self.corrupted(decoder, "a curve with no points must be serialized as null")
        }

        // Rust sorts the samples and then takes coverage from the ends of that
        // same vector, so these are equalities rather than bounds checks.
        guard coverage.sampleCount == points.count else {
            throw Self.corrupted(decoder, "coverage.sampleCount disagrees with points")
        }
        guard coverage.oldestSampledAt == first.sampledAt,
              coverage.newestSampledAt == last.sampledAt
        else {
            throw Self.corrupted(decoder, "coverage does not match the points it covers")
        }
        guard zip(points, points.dropFirst()).allSatisfy({ $0.sampledAt <= $1.sampledAt }) else {
            throw Self.corrupted(decoder, "points are not ordered by sampledAt")
        }

        // `validate_series` keys admission on `(normalized reset, phase bucket)`
        // and refuses a repeat, so no two retained samples can be identical.
        // Only the exact-repeat half of that is mirrored here, on purpose: the
        // full key needs `normalize_reset`'s quantum rounding and
        // `phase_bucket`'s f64 arithmetic, and a Swift re-implementation of
        // those would be its own drift surface — one whose failure mode is
        // rejecting a *valid* curve, which is worse than the malformed one it
        // would catch. This repository has already paid for simulating another
        // language's floating-point rounding once (the printf/Math.Round
        // cross-check drift in verification.md). The exact-repeat check needs
        // only integers.
        var seen = Set<SampleKey>()
        guard points.allSatisfy({
            seen.insert(SampleKey(
                sampledAt: $0.sampledAt, resetAt: $0.resetAt,
                durationSeconds: $0.durationSeconds)).inserted
        }) else {
            throw Self.corrupted(decoder, "a sample is repeated in the curve")
        }

        // The store caps one series at `MAX_SAMPLES`, and unique `(reset,
        // bucket)` keys cap each cycle at the 48 buckets that exist. Both are
        // integers, so the ceiling is mirrored while the key is not.
        guard points.count <= Self.maxPoints else {
            throw Self.corrupted(decoder, "the curve holds more samples than one series can")
        }
    }
}

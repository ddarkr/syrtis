import Foundation

/// Past quota windows, each joined to what was actually spent inside it.
///
/// The window card answers "this window"; this answers "the windows before
/// it", and the join is the point. Measured 2026-08-16 on live data, a
/// `claude/session.v1` window moved **1%** while 308M tokens and $535 of
/// API-equivalent value were recorded in the same five hours — because almost
/// all of it was `gpt-5.6-sol`, which is charged to a different subscription.
/// A history that showed either half alone would be actively misleading.

/// One recorded reset cycle, derived from the persisted quota curve alone.
public struct QuotaCycle: Equatable, Sendable {
    /// Reset instant in ms. Doubles as the cycle's identity — the engine groups
    /// its samples by exactly this value.
    public let resetAtMs: Int64
    public let startMs: Int64
    /// How much of the allowance this cycle consumed: the distance the readings
    /// travelled, summed over their rises, NOT the highest reading alone and
    /// NOT the range between the lowest and the highest.
    ///
    /// Counting only rises is not cosmetic. Sampling starts whenever the app
    /// happens to be running, so a cycle first observed at 40% used would report
    /// 40% as its consumption when the app only witnessed the last few points of
    /// it. What travelled between readings is what this app can honestly claim
    /// to have seen.
    ///
    /// The range was the earlier answer and agrees with this one on every cycle
    /// whose readings only rise. It stops agreeing when a reset falls inside the
    /// group: the readings return to zero and the range then saturates at the
    /// highest reading while the consumption keeps going. See
    /// `QuotaHistoryFold.consumed`, which states the rule once for both this and
    /// the live single-window path.
    ///
    /// Note the floor this leaves: `QuotaCurvePoint` rejects `usedPercent == 0`
    /// at decode, so a cycle's first reading is always above zero and the
    /// distance necessarily omits whatever was consumed before it. That
    /// understates, and understating is the right direction — the alternative
    /// assumes the app witnessed a window it may have joined late.
    ///
    /// May exceed 100 when a group holds more than one window's worth of
    /// consumption, which is the deliberate outcome of keeping a window keyed by
    /// its reset time rather than splitting it at a reset the provider never
    /// reported.
    public let usedPercent: Double
    /// How many separately measured rises `usedPercent` sums. One on a cycle
    /// whose readings only rose; more when a reset fell inside the group. The
    /// admission bar and the quoted error scale by it, because each rise was
    /// quantised on its own. See `QuotaHistoryFold.risingRuns`.
    public let risingRuns: Int
    /// The highest absolute reading seen in this cycle.
    ///
    /// Separate from `usedPercent`, which is the observed SPAN. The two answer
    /// different questions and only coincide when the app watched the cycle
    /// from zero: a cycle first seen at 40% and last seen at 100% consumed 60
    /// points as far as this machine can tell, and reached the ceiling. Deriving
    /// "never ran out" from the span called that cycle a quiet one.
    public let peakUsedPercent: Double
    public let sampleCount: Int
    /// The instants of the first and last reading in this cycle.
    ///
    /// Carried because a ratio is only meaningful when its numerator and
    /// denominator cover the same interval, and the denominator is a span
    /// between two readings — not the whole window. Usage from a stretch the
    /// app was not running for would otherwise be counted against quota
    /// movement nobody observed. `WindowEquivalence.row` already had this
    /// right; the aggregate over cycles did not, and it cost 8 points of
    /// spread on live data (46% to 38%).
    public let firstSampleMs: Int64
    public let lastSampleMs: Int64

    /// How far back this cycle's evidence reaches — the earlier of where the
    /// window is computed to have started and where sampling actually began.
    ///
    /// Normally `startMs`, since the first reading lands after the window
    /// opens. They invert when the provider SHORTENS its reported duration
    /// mid-cycle: `startMs` is derived from the newest point's duration, so a
    /// window that went from seven days to five moves its own start forward,
    /// past readings already taken. Bounding a message scan at `startMs` would
    /// then drop usage from before it while `usedPercent` — the span between
    /// the first and last reading — still counts the movement those readings
    /// showed. Numerator short, denominator whole.
    public var evidenceStartMs: Int64 { min(startMs, firstSampleMs) }
    /// Fraction of the window the samples actually cover, 0...1. A cycle
    /// observed for eight minutes of five hours is not evidence about that
    /// cycle, and the UI has to be able to say so.
    public let observedFraction: Double

    public var durationMs: Int64 { resetAtMs - startMs }

    public init(
        resetAtMs: Int64, startMs: Int64, usedPercent: Double,
        sampleCount: Int, observedFraction: Double,
        firstSampleMs: Int64 = 0, lastSampleMs: Int64 = 0,
        peakUsedPercent: Double? = nil,
        risingRuns: Int = 1
    ) {
        self.peakUsedPercent = peakUsedPercent ?? usedPercent
        self.risingRuns = risingRuns
        self.resetAtMs = resetAtMs
        self.startMs = startMs
        self.usedPercent = usedPercent
        self.sampleCount = sampleCount
        self.observedFraction = observedFraction
        self.firstSampleMs = firstSampleMs
        self.lastSampleMs = lastSampleMs
    }
}

/// A cycle plus what was spent inside it, split by whether it counted against
/// the subscription this history belongs to.
public struct QuotaHistoryRow: Equatable, Sendable, Identifiable {
    public let cycle: QuotaCycle
    /// Attributed to THIS subscription — the number the quota bar is about.
    public let mineTokens: Int64
    /// The same total on the BARS' basis — cache reads removed.
    ///
    /// Not the equivalence's basis. `spanTokens` below is the full count, for
    /// the reason given there; this one matches `WindowCardGeometry`, which
    /// sizes the bars from everything except cache reads because including
    /// them decouples the bars from the quota line (measured: 4.5x vs 1.2x
    /// discrimination). A figure printed beside the bars should be countable
    /// in them.
    public let mineTokensExCacheRead: Int64
    /// `mineTokens` split by token class, for the row's hover breakdown.
    ///
    /// Five independently accumulated lanes. The compact producer preserves
    /// per-row total overflow separately from signed aggregate lane saturation.
    public let mineBreakdown: TokenBreakdown
    public let mineCost: Double
    /// The same two quantities restricted to the cycle's OBSERVED span, which
    /// is the only interval the quota delta describes. The whole-window figures
    /// above are what the row displays — "this window cost me X" is a question
    /// about the window — while these are the only ones a ratio may divide.
    ///
    /// The FULL token count, cache reads included, and that is the load-bearing
    /// part. These two feed `WindowEquivalence`, which prints them on one line
    /// as "10% of quota ~ X tokens · $Y API-equivalent". The compact producer
    /// supplies the whole priced cost, not a cost decomposed by token class,
    /// so equivalence must use the full token count beside it.
    ///
    /// Full-and-full is therefore not a preference between two workable
    /// options; it is the only achievable pairing until per-class cost crosses
    /// the FFI. See issue #237.
    public let spanTokens: Int64
    public let spanCost: Double
    /// Everything else recorded in the same interval, summed. Not noise: it is
    /// the answer to "the window barely moved, so where did the work go".
    ///
    /// THREE attribution states, not one. A message lands here when the user
    /// declared it against another subscription, when they declared it excluded,
    /// and when they have not classified it at all — and the card called the
    /// whole bucket "other subscriptions", which on an undeclared setup is a
    /// claim about every message this machine recorded. `otherAssigned` carries
    /// the part that claim is true of, so the copy can name what it knows.
    public let otherTokens: Int64
    public let otherCost: Double
    /// Which of those states are PRESENT in the bucket, as facts rather than
    /// as totals.
    ///
    /// Deliberately not a token count. The first version carried
    /// `otherAssignedTokens` and the label compared it against `otherTokens` —
    /// which is a comparison in one dimension over data that has two, so an
    /// unclassified row carrying cost and no tokens made the totals equal and
    /// the whole bucket read as "other subscriptions" while some of the spend
    /// was unclassified. A presence flag cannot be fooled by which dimension a
    /// contribution happens to arrive in.
    public let otherHasAssigned: Bool
    /// Declared EXCLUDED by the user, kept apart from never-classified.
    ///
    /// Both are "not this subscription's", so the fold sent them to one flag
    /// and the card labelled the result "Unclassified usage" — telling the user
    /// their own explicit exclusion was work they had not got round to
    /// classifying. Excluding a source IS classifying it; the difference
    /// between "I dealt with this" and "there is something here for you to
    /// deal with" is the only thing this line is read for.
    public let otherHasExcluded: Bool
    public let otherHasUnattributed: Bool
    /// This subscription's models, largest first.
    public let models: [QuotaHistoryModel]

    public var id: Int64 { cycle.resetAtMs }
}

public struct QuotaHistoryModel: Equatable, Sendable, Identifiable {
    /// Carried alongside the model because `ModelColorMap.color` keys on the
    /// pair. Without it the segments here would be coloured by a different rule
    /// than the same models in the model breakdown and the usage chart, and one
    /// model would be two colours depending on which card you were looking at.
    public let providerId: String
    public let modelId: String
    public let tokens: Int64
    /// `tokens` split by class, on the same five lanes as the row's. A model
    /// line and the row above it are the same question at two scopes, so a
    /// reader who can break one down and not the other is being told the
    /// detail exists only sometimes.
    public let breakdown: TokenBreakdown
    public let cost: Double

    public var id: String { "\(providerId)|\(modelId)" }
}

public enum QuotaHistoryFold {
    /// Groups curve points into cycles, newest first.
    ///
    /// `durationSeconds` is per point rather than per cycle, so the cycle's
    /// length is taken from its own newest point: a window whose provider changed
    /// its reported duration mid-cycle should be placed by what it reports now,
    /// not by what it reported when sampling began.
    /// Completed cycles only.
    ///
    /// The running cycle is excluded, and each point says for itself whether it
    /// is in it. Folding it in put the running cycle in a strip captioned "past
    /// windows", let a partially observed span stand beside completed ones, and
    /// let it count toward the three-cycle threshold the equivalence needs — an
    /// estimate that would then change under the reader as the cycle filled.
    ///
    /// The first version took the curve's `activeResetAt` and compared it to
    /// each point's `resetAt`. Those are not comparable values: the engine
    /// publishes the RAW provider reset there while every stored `resetAt` has
    /// been through `normalize_sample_reset`, so the exclusion silently did
    /// nothing whenever the provider's reset was off the quantum — which is
    /// most of the time. `isActiveGroup` is the producer's own answer, so there
    /// is no longer a parameter a caller can omit and no rule stated twice in
    /// two languages.
    ///
    /// Returns EVERY recorded cycle. The cap that bounds the message scan is
    /// `considered(_:)`, applied by the consumers that pay for a scan — not
    /// here. Putting it here looked like the tidier place ("bound it once, at
    /// the source") and was wrong: `QuotaOverviewFold.summaries` derives
    /// LIFETIME facts from this array — `peakPercent`, `neverExhausted`,
    /// `cycleCount` — and costs no scan at all. A capped fold made a window
    /// that ran out thirty-three cycles ago report that it never had.
    public static func cycles(points: [QuotaCurvePoint]) -> [QuotaCycle] {
        var grouped: [Int64: [QuotaCurvePoint]] = [:]
        for point in points where !point.isActiveGroup {
            // Keyed on the canonical reset, not the stored one. The engine
            // normalises a stored `resetAt` onto a quantum before writing it
            // (`normalize_reset`), which is what makes an exact key workable at
            // all — but that normalisation is clamped to `[sampled_at, ...]`
            // afterwards, and for the poll that lands AT a reset the clamp
            // pushes the value back off the quantum. Measured on a real store:
            // resets 1789897140 and 1789897145 five seconds apart, the second
            // carrying one sample whose `sampledAt` equals its `resetAt`. That
            // sample is the tail of the cycle it was excluded from, so the
            // reader saw one window twice — once correct, once at 0% consumed
            // with the whole span's tokens beside it.
            //
            // Reapplying the quantum here is not a second rule. It is the same
            // rule, applied where the clamp cannot undo it, and it is a read of
            // data already written: nothing is migrated and no store is
            // rewritten. The engine's own normalisation stays the producer's
            // job; this only stops a value that escaped it from splitting a
            // cycle in the one consumer that groups by it.
            grouped[canonicalReset(point.resetAt, point.durationSeconds), default: []]
                .append(point)
        }

        return grouped.compactMap { _, raw -> QuotaCycle? in
            let sorted = raw.sorted { $0.sampledAt < $1.sampledAt }
            guard let last = sorted.last, let first = sorted.first,
                  last.durationSeconds > 0
            else { return nil }
            // The group's own reset, not the quantised key it was grouped
            // under. The key exists to decide WHICH points belong together;
            // reporting it would move every window this fold returns by up to
            // half a quantum, including the overwhelming majority that never
            // drifted at all.
            //
            // The mode, because a drifted reading is by construction the rare
            // one: the split this repairs was 47 samples against 1. Ties go to
            // the smaller value so the result cannot depend on dictionary
            // order.
            var tally: [Int64: Int] = [:]
            for point in sorted { tally[point.resetAt, default: 0] += 1 }
            guard let resetAt = tally.max(by: {
                $0.value != $1.value ? $0.value < $1.value : $0.key > $1.key
            })?.key else { return nil }
            let used = sorted.map(\.usedPercent)
            let start = resetAt - last.durationSeconds
            return QuotaCycle(
                resetAtMs: resetAt * 1000,
                startMs: start * 1000,
                usedPercent: consumed(used),
                sampleCount: sorted.count,
                observedFraction: min(1, max(0, Double(last.sampledAt - first.sampledAt)
                    / Double(last.durationSeconds))),
                firstSampleMs: first.sampledAt * 1000,
                lastSampleMs: last.sampledAt * 1000,
                peakUsedPercent: used.max() ?? 0,
                risingRuns: risingRuns(used))
        }
        .sorted { $0.resetAtMs > $1.resetAtMs }
    }

    /// The quantum a stored `resetAt` is meant to sit on.
    ///
    /// Mirrors `normalize_reset` in `agent_quota_history.rs` deliberately and
    /// must move with it: `clamp(duration / 100, 60, 300)`, rounded half up.
    /// Stated here rather than inferred, because a tolerance that drifts from
    /// the producer's would either merge two real windows or stop merging the
    /// split this exists for, and neither failure announces itself.
    ///
    /// Well below any window length by construction — a hundredth of it,
    /// capped at five minutes — so two adjacent cycles can never collapse into
    /// one. The five-hour session window quantises at 180 seconds against an
    /// 18000-second spacing.
    static func canonicalReset(_ resetAt: Int64, _ durationSeconds: Int64) -> Int64 {
        let quantum = max(min(max(durationSeconds / 100, 0), 300), 60)
        guard quantum > 0 else { return resetAt }
        let quotient = resetAt / quantum
        let remainder = resetAt % quantum
        // Round half up, matching the Rust side's `remainder * 2 >= quantum`.
        // `%` and `/` agree with `rem_euclid`/`div_euclid` for a positive
        // quantum and a non-negative epoch, which every stored reset is.
        return (remainder * 2 >= quantum ? quotient + 1 : quotient) * quantum
    }

    /// The newest cycles a scan-paying surface may look at.
    ///
    /// Applied by the consumers whose cost grows with the answer: the history
    /// card's cycle list, which bounds the union scan through its oldest
    /// entry's `evidenceStartMs`, and the admitted set behind the equivalence.
    /// Lifetime summaries deliberately do not call this.
    public static func considered(_ cycles: [QuotaCycle]) -> [QuotaCycle] {
        Array(cycles.prefix(consideredCycles))
    }

    /// How far back any cycle-derived surface reaches, in cycles.
    ///
    /// The engine retains 128 cycles per series and this fold used to return
    /// all of them, but nothing downstream wants that many: the history card
    /// draws 12 rows and the overview strip 16. The cost of the extra ones is
    /// not the list, it is that the OLDEST cycle sets where the message scan
    /// starts — `min(windowStart, cycles.last.startMs)` — so a 5-hour session
    /// window at 128 cycles asked for a 26-day scan to render twelve rows, and
    /// a weekly window walked that start backwards for ever. Capping the fold
    /// bounds the scan as a consequence, which is why the cap lives here and
    /// not at each consumer.
    ///
    /// 32, not the 16 the issue proposed. `--window-probe` swept the cap over
    /// this machine's real history on 2026-08-21: at 20 and above the session
    /// window reports ~650-690k tokens per 10% with an 8-10% error bar, and at
    /// 12 and below it reports no figure at all, only a 242k-955k spread.
    ///
    /// 16 is the cliff edge, and the sweep caught it moving. Two runs an hour
    /// apart, differing by one newly completed cycle, put 16 on opposite sides:
    /// `spread` at 27 recorded cycles, `ratio` at 28. A cap whose output flips
    /// between "here is the number" and "we cannot say" as one ordinary cycle
    /// closes is not a bound, it is a coin toss the user watches. 16 is enough
    /// for the jackknife in principle and is not on the data, because admitted
    /// cycles are a subset of recorded ones and the pool empties faster than
    /// the count suggests.
    ///
    /// 32 does not bite on any window here today (the widest has 27), which is
    /// the point: it bounds the growth without moving a number anyone reads.
    ///
    /// The surface that has to fit inside it is the history card's row list,
    /// which draws `QuotaHistoryCard.visibleRows`; a cap below that would
    /// silently draw fewer rows than the card intends, so a selftest asserts
    /// the fit. NOT `QuotaOverviewFold.stripLength`, which an earlier version
    /// of this comment named: the overview strip reads the uncapped `collected`
    /// list, so the cap never reaches it.
    public static let consideredCycles = 32

    /// Reconstructs presentation rows from complete compact cycle jobs. The
    /// evidence interval and observed span were folded independently natively.
    public static func rows(
        cycles: [QuotaCycle], results: [WindowUsageBatch.Cycle]
    ) throws -> [QuotaHistoryRow] {
        let matched = try matchedResults(cycles: cycles, results: results)
        return zip(cycles, matched).map { cycle, result in
            QuotaHistoryRow(
                cycle: cycle,
                mineTokens: result.mineTokens,
                mineTokensExCacheRead: result.mineTokensExCacheRead,
                mineBreakdown: result.mineBreakdown,
                mineCost: result.mineCost,
                spanTokens: result.spanTokens, spanCost: result.spanCost,
                otherTokens: result.otherTokens, otherCost: result.otherCost,
                otherHasAssigned: result.otherHasAssigned,
                otherHasExcluded: result.otherHasExcluded,
                otherHasUnattributed: result.otherHasUnattributed,
                models: result.models.map {
                    QuotaHistoryModel(
                        providerId: $0.providerId, modelId: $0.modelId,
                        tokens: $0.tokens, breakdown: $0.breakdown, cost: $0.cost)
                }.sorted {
                    if $0.tokens != $1.tokens { return $0.tokens > $1.tokens }
                    if $0.cost != $1.cost { return $0.cost > $1.cost }
                    return ($0.providerId, $0.modelId) < ($1.providerId, $1.modelId)
                })
        }
    }

    /// The estimator consumes only observed full tokens/cost, never the
    /// evidence-window display totals. Admission and weighting remain Swift.
    public static func equivalenceCycles(
        cycles: [QuotaCycle], results: [WindowUsageBatch.Cycle]
    ) throws -> [WindowEquivalence.Cycle] {
        let matched = try matchedResults(cycles: cycles, results: results)
        return zip(cycles, matched).map { cycle, result in
            WindowEquivalence.Cycle(
                deltaPercent: cycle.usedPercent,
                spanTokens: result.spanTokens, spanCost: result.spanCost,
                observedFraction: cycle.observedFraction, risingRuns: cycle.risingRuns)
        }
    }

    private static func matchedResults(
        cycles: [QuotaCycle], results: [WindowUsageBatch.Cycle]
    ) throws -> [WindowUsageBatch.Cycle] {
        var byReset: [Int64: WindowUsageBatch.Cycle] = [:]
        for result in results {
            guard result.state == "covered", byReset[result.resetAtMs] == nil else {
                throw incompleteCycleResults()
            }
            byReset[result.resetAtMs] = result
        }
        guard results.count == cycles.count,
              Set(cycles.map(\.resetAtMs)).count == cycles.count
        else { throw incompleteCycleResults() }
        return try cycles.map { cycle in
            guard let result = byReset[cycle.resetAtMs] else {
                throw incompleteCycleResults()
            }
            return result
        }
    }

    private static func incompleteCycleResults() -> NSError {
        NSError(domain: "WindowUsageBatch", code: 1, userInfo: [
            NSLocalizedDescriptionKey: "Incomplete quota cycle usage results.",
        ])
    }

    /// How much allowance a run of readings records consuming: the distance
    /// they travelled, summed over the rises between consecutive readings.
    ///
    /// Neither the range nor the displacement, though on a cycle whose readings
    /// only rise all three are the same number. They stop agreeing the moment a
    /// reset falls inside the group: the readings return to zero, so the
    /// displacement collapses toward nothing while the range saturates at
    /// whatever the highest reading happened to be. Measured on a real merged
    /// group — 8 by displacement, 85 by range, 93 actually travelled. The
    /// numerator keeps every message from both sides of that reset whichever
    /// denominator is used, so one that does not accumulate makes the ratio
    /// report a multiple of the truth: 11.6x on that group, which is where a
    /// card reading `$2974.92` came from.
    ///
    /// Only rises count, which keeps the property the range was chosen for.
    /// Sampling starts whenever the app happens to be running, so a cycle first
    /// seen at 40% must not claim the 40% nobody watched.
    ///
    /// Order-dependent where the range was not. Every caller passes readings
    /// already sorted by sample time; one that did not used to get the right
    /// answer by accident and now gets a wrong one.
    ///
    /// Every rise counts, including one that only undoes a provider's downward
    /// correction: `[0, 40, 35, 40]` returns 45 where 40 was consumed. That is
    /// a known overstatement and it is not fixable from the readings — a
    /// decline does not say whether the quota was refilled or the earlier
    /// number was wrong, and neither its size nor where it lands separates the
    /// two. Measured across every series in a real store: persistent drops run
    /// 2 to 100 points and bounce-backs 2 to 22, fully overlapping; real resets
    /// land anywhere from 0% to 21%, with bounce-backs landing at 0% as well.
    ///
    /// The numerator has the twin of this, and it is left in place for the
    /// same reason. A declining interval contributes nothing here while every
    /// message inside it still counts above the line, so whatever was consumed
    /// between the last reading before a reset and the reset itself is divided
    /// by the rises around it. Excluding those messages is right when the
    /// decline was a reset and wrong twice over when it was a correction —
    /// numerator down, denominator already up — and the paragraph above is the
    /// measurement saying the two cannot be told apart. Bound, from the same
    /// store: 2 of 128 groups contain any decline at all, declining intervals
    /// hold 0.17% of all observed time, 4.08% in the worst single group and
    /// 0.92% in the merged group this whole change was written for. Time, not
    /// messages — the message share is not measured, and a gap denser than
    /// average carries proportionally more.
    ///
    /// It is kept because the bound is small and the alternative costs more.
    /// Over 121 groups in that store, this and the range agreed on 119 — every
    /// cycle whose readings only rise — and differed by at most 3.5% on the
    /// two that had been refilled, where the range prices the window at its
    /// widest excursion rather than at what it consumed. The residual also
    /// leans the safe way: overstating the denominator understates the ratio,
    /// so a quota reads as worth less rather than more.
    public static func consumed(_ readings: [Double]) -> Double {
        zip(readings, readings.dropFirst()).reduce(0.0) { $0 + max(0, $1.1 - $1.0) }
    }

    /// How many separately measured rises `consumed` summed: the number of
    /// times the readings started going up after not going up. Zero on
    /// readings that never rose.
    ///
    /// The provider reports whole percents, so each rise carries the same
    /// ±0.5 quantisation whether it spans 3 points or 90. Summing two rises
    /// sums their uncertainty as well, and a ratio built on `[0, 3, 0, 3]`
    /// is not a 6-point measurement at ±0.5 — it is two 3-point measurements
    /// at ±0.5 each, neither of which clears the bar on its own. Callers that
    /// quote an error or gate on a minimum delta scale both by this count, so
    /// a group that crossed a reset does not come out looking more precise
    /// than a group that did not.
    public static func risingRuns(_ readings: [Double]) -> Int {
        // A positive delta starts a run iff no earlier delta was positive, or
        // the most recent non-zero delta before it was negative. A zero delta
        // is transparent: a plateau inside a rise is still one measurement
        // span under the ±0.5-per-reading model, so it neither starts nor ends
        // a run. Counting declines instead — the first cut here — charged a
        // trailing decline as a run that contributed no rise.
        var runs = 0
        var lastNonZeroWasNegative = true
        for (before, after) in zip(readings, readings.dropFirst()) {
            if after > before {
                if lastNonZeroWasNegative { runs += 1 }
                lastNonZeroWasNegative = false
            } else if after < before {
                lastNonZeroWasNegative = true
            }
        }
        return runs
    }

}

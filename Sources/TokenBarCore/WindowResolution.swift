public enum WindowResolution: Equatable, Sendable {
    case active(start: Int64, end: Int64)
    case idle
    case inferred(start: Int64, end: Int64)
    case unavailable
}

public enum WindowResolver {
    public static func resolve(
        resetsAtMs: Int64?,
        durationMs: Int64?,
        now: Int64,
        firstUsageAfterReset: Int64?
    ) -> WindowResolution {
        guard let reset = resetsAtMs, let duration = durationMs else {
            return .unavailable
        }

        // R1 and R4: the provider anchor is only usable within one window
        // length, in either direction. Backwards it bounds the usage probe;
        // forwards it catches a clock that ran ahead — issue #144's root cause
        // was exactly one forward skew, latched by a monotonic max().
        if reset > now {
            guard reset - now <= duration else { return .unavailable }
            return .active(start: reset - duration, end: reset)
        }
        guard now - reset <= duration else { return .unavailable }
        guard let start = firstUsageAfterReset else { return .idle }
        return .inferred(start: start, end: start + duration)
    }
}

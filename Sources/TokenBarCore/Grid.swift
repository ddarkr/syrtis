import Foundation

// GitHub-style contribution grid layout, port of src/lib/grid.ts. Pure
// civil-date math over ISODay — no Calendar/timezone involvement, matching
// how syrtis-core buckets days.

public struct GridCell: Sendable {
    public let col: Int
    /// 0 = Sunday (top row).
    public let row: Int
    public let date: String
    public let inYear: Bool
    public let active: Bool
    public let tokens: Int64
    public let cost: Double
}

public struct GridLayout: Sendable {
    public let cols: Int
    public let rows: Int // 7
    public let cells: [GridCell]
    public let maxTokens: Int64
}

extension ISODay {
    /// Day of week, Sunday = 0. Day 0 (1970-01-01) was a Thursday.
    public var weekday: Int { ((number + 4) % 7 + 7) % 7 }
}

/// 53+ columns x 7 rows; col 0 row 0 = the Sunday on or before Jan 1 of
/// `year`. Cells outside the year are `inYear: false` and never active.
public func buildGrid(year: String, perDayMap: [String: PerDay]) -> GridLayout {
    guard let jan1 = ISODay("\(year)-01-01"), let dec31 = ISODay("\(year)-12-31")
    else { return GridLayout(cols: 53, rows: 7, cells: [], maxTokens: 0) }
    let start = ISODay(number: jan1.number - jan1.weekday)
    let lastCol = (dec31.number - start.number) / 7
    let cols = max(53, lastCol + 1)
    let yearPrefix = "\(year)-"

    var cells: [GridCell] = []
    cells.reserveCapacity(cols * 7)
    var maxTokens: Int64 = 0
    for col in 0..<cols {
        for row in 0..<7 {
            let date = ISODay(number: start.number + col * 7 + row).iso
            let inYear = date.hasPrefix(yearPrefix)
            let entry = perDayMap[date]
            let tokens = entry?.tokens ?? 0
            let active = inYear && tokens > 0
            if active { maxTokens = max(maxTokens, tokens) }
            cells.append(
                GridCell(
                    col: col, row: row, date: date, inYear: inYear,
                    active: active, tokens: tokens, cost: entry?.cost ?? 0))
        }
    }
    return GridLayout(cols: cols, rows: 7, cells: cells, maxTokens: maxTokens)
}

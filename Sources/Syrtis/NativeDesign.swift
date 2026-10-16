import SwiftUI

/// Shared native presentation values. Fixed stored constants only — no theme
/// engine, no strings, no preference keys, no timers, no caches.
enum NativeDesign {
    static let spacing: CGFloat = 12
    static let cardPadding: CGFloat = 14
    static let cornerRadius: CGFloat = 14
    static let bodyFont: Font = .system(size: 13)
    static let detailFont: Font = .system(size: 12)
    static let sectionFont: Font = .system(size: 14, weight: .semibold)
    static let titleFont: Font = .system(size: 22, weight: .semibold)
}

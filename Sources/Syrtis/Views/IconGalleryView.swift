import SwiftUI
import TokenBarCore

/// Debug-only grid of every registered client's icon, rendered through the
/// real AgentIconView / NSImage(contentsOf:) path (the one that caught the
/// Antigravity CoreSVG mask+blur bug) rather than a browser mockup. Not
/// wired to any real usage data — `--icon-gallery` opens it standalone so
/// the whole roster can be screenshotted without needing local session
/// fixtures for every client.
struct IconGalleryView: View {
    /// Initial window content size; the window is resizable down to `minSize`.
    static let contentSize = CGSize(width: 900, height: 820)
    static let minSize = CGSize(width: 560, height: 420)

    private static let ids = [
        "claude", "gemini", "opencode", "copilot", "qwen",
        "codex", "droid", "kilocode", "kilo", "synthetic", "kiro", "codebuff",
        "cursor", "warp", "amp", "pi", "kimi",
        "cline", "jcode", "micode", "gjc", "grok",
        "antigravity", "antigravity-cli", "grok-bot",
        "hermes", "roocode", "mux", "crush", "goose", "zed", "trae", "openclaw",
        "zcode", "augment", "hindsight", "muse", "reasonix",
        "kimchi", "senpi", "omp",
    ]

    private let columns = [GridItem(.adaptive(minimum: 110), spacing: 16)]
    var body: some View {
        ScrollView {
            LazyVGrid(columns: columns, spacing: 20) {
                ForEach(Self.ids, id: \.self) { id in
                    VStack(spacing: 6) {
                        AgentIconView(clientId: id, size: 48)
                        AgentIconView(clientId: id, size: 14)
                        Text(id)
                            .font(.caption.monospaced())
                            .lineLimit(1)
                            .truncationMode(.middle)
                        Text(ClientRegistry.style(id).displayName)
                            .font(NativeDesign.detailFont)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                            .multilineTextAlignment(.center)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .frame(width: 110)
                    .help(ClientRegistry.style(id).displayName)
                    .accessibilityElement(children: .combine)
                    .accessibilityLabel("\(ClientRegistry.style(id).displayName), \(id)")
                }
            }
            .padding(20)
        }
        .frame(
            minWidth: Self.minSize.width,
            minHeight: Self.minSize.height)
    }
}

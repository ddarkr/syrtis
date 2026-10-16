import SwiftUI

/// The eight-lens row under the header, port of ViewSwitch.tsx. WHAT axis,
/// visually distinct from the client WHO row above it (glass selected pill
/// on a quiet row): a single floating glass bar with a quiet sliding thumb.
///
/// All eight lenses stay reachable at 400pt via native horizontal overflow
/// with the selection scrolled into view — no minimumScaleFactor cram.
/// Order, raw values and bindings are untouched: the row renders `views`
/// in the given order and writes the bound `AppView` directly.
struct ViewSwitch: View {
    @Binding var active: AppView
    let views: [AppView]

    @Namespace private var thumb
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 2) {
                    ForEach(views, id: \.self) { view in
                        Button {
                            active = view
                        } label: {
                            Text(view.label)
                                .font(active == view ? NativeDesign.bodyFont.weight(.semibold) : NativeDesign.bodyFont)
                                .foregroundStyle(active == view ? .primary : .secondary)
                                .lineLimit(1)
                                .fixedSize()
                                .padding(.horizontal, 8)
                                .padding(.vertical, 4)
                                .modifier(SelectionBackground(
                                    isSelected: active == view,
                                    shape: RoundedRectangle(cornerRadius: 6), namespace: thumb))
                                .contentShape(RoundedRectangle(cornerRadius: 6))
                        }
                        .buttonStyle(.plain)
                        .accessibilityAddTraits(active == view ? .isSelected : [])
                        .id(view)
                    }
                }
                .padding(2)
            }
            .onAppear { proxy.scrollTo(active, anchor: nil) }
            .onChange(of: active) { _, next in
                if reduceMotion {
                    proxy.scrollTo(next, anchor: nil)
                } else {
                    withAnimation(.easeOut(duration: 0.15)) {
                        proxy.scrollTo(next, anchor: nil)
                    }
                }
            }
        }
        .padding(2)
        // The bar itself is the floating navigation glass; the thumb keeps
        // the shared quiet fill (glass nested in glass renders murky).
        // navigationGlass owns the 26+/14/accessible fallbacks.
        .navigationGlass(cornerRadius: 10)
        .panelSelectionSlide(active)
    }
}

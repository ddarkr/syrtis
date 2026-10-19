---
status: active
id: kb-native-design
kind: canonical
scope: repository
read_when: changing Settings, dashboard lenses, cards, typography, glass, motion, or window lifecycle
last_verified: 2026-10-19
sources: ["Sources/Syrtis/NativeDesign.swift", "Sources/Syrtis/SettingsWindowController.swift", "Sources/Syrtis/Views/SettingsWindowView.swift", "Sources/Syrtis/Views/Cards.swift", "Sources/Syrtis/Views/ViewSwitch.swift", "Sources/Syrtis/Views/DashboardTabs.swift", "Sources/Syrtis/GlassPanelPresenter.swift", "Sources/Syrtis/Views/SettingsPanel.swift", "Sources/Syrtis/IconGalleryWindowController.swift"]
---

# Apple Liquid Glass visual contract

## 文件目的

這份文件是 Settings、menu-bar dashboard、lenses、cards 與 onboarding 的 visual／interaction contract。它只擁有呈現層：data engine、C ABI、stored preference keys and values、routes、card／client hiding and order、quotas、attribution semantics、last-good／stale distinctions、keyboard shortcuts、lifecycle 與 session cancellation 全部維持不變，不新增 polling、cache 或 engine 改動。

## 目錄

- [Shared tokens](#shared-tokens)
- [Settings shell](#settings-shell)
- [Dashboard shell](#dashboard-shell)
- [Lens inventory](#lens-inventory)
- [Typography and controls](#typography-and-controls)
- [Surfaces and glass](#surfaces-and-glass)
- [Motion and lifecycle](#motion-and-lifecycle)
- [Preserved semantics](#preserved-semantics)

---

## Shared tokens

[`NativeDesign`](../../Sources/Syrtis/NativeDesign.swift) 只提供 fixed stored constants，沒有 framework 或 theme engine：`spacing` 12、`cardPadding` 14、`cornerRadius` 14；`bodyFont` 13、`detailFont` 12、`sectionFont` 14 semibold、`titleFont` 22 semibold；system colors、system fonts、monospaced digits。

## Settings shell

[`SettingsWindowView`](../../Sources/Syrtis/Views/SettingsWindowView.swift) 使用系統 `NavigationSplitView`、sidebar `List(selection:)` 與 grouped `Form`／`Section`。Sidebar 材質、收合按鈕與 unified toolbar 由系統管理，不以 custom HStack、浮動玻璃矩形或手刻 toolbar 模擬。Detail Form 是單一捲動容器；不要對整個 Form 設定窄幅 frame，否則 native scroll-edge surface 可能遮住 toolbar 標題。表單頂部不放空白佔位 row，Section 內也不以獨立 Divider 生成空白設定行。標題使用 `.navigationTitle`；原生 menu Picker 與 switch Toggle 保留系統控制項外觀。不要對互動表單套用 `backgroundExtensionEffect()`，它會鏡射內容而非提供合適的設定背景。新字串必須具備 en／zh-Hans／zh-Hant entries。

Sidebar 使用六個 page，以兩個無標題 native Section 分開前三個功能頁與 Disk Usage／General／About；不為少量項目增加階層或自製 selected pill。SF Symbols 使用系統 accent color，選取時使用 semantic primary foreground；row 字體與高度不固定，讓 `Label` 自動跟隨 macOS 的 [sidebarRowSize](https://developer.apple.com/documentation/swiftui/environmentvalues/sidebarrowsize) 偏好。底部為 compact native `Menu`：app mark／name 作為入口，選單內顯示 bundle version、GitHub 與 Sponsor；不再使用自製 hover chip、cursor tracking 或多行品牌面板。材質、menu tracking、鍵盤與 focus appearance 由系統管理，無額外 glass 疊層。

[`DiskUsageView`](../../Sources/Syrtis/Views/DiskUsageView.swift) stays inside that single grouped Form. Show the allocated total, disposable-cache size, a horizontal composition bar and matching named rows with sizes, percentages and explanatory copy. Color is supplementary: category names, exact-byte help and accessibility values remain available. Client details disclose stored record counts and date ranges, never a fabricated division of shared database bytes. Include the measured time, selectable usage-directory path and Finder action. Refresh is manual or on first mount, without a storage polling timer. Failures retain the previous measurement and label it stale; initial loading, empty storage and unavailable data sources stay distinct. Safe maintenance and destructive reset use separate sections, with explicit confirmation explaining source re-collection and unrecoverable source-free history.

[`SettingsWindowController`](../../Sources/Syrtis/SettingsWindowController.swift) 在 macOS 26+ 使用 public `NSHostingSceneRepresentation`、`NSApplication.addSceneRepresentation` 與 data-driven `WindowGroup`；AppDelegate 在 will-finish-launching 階段註冊 scene。固定 Codable identity 開啟初始窗，已綁定的視窗直接更新 destination 並取得焦點，不為每次導向重新呼叫 openWindow。macOS 14–25 保留 NSHostingController fallback。初始尺寸 1040x700、root minimum 800x560；native toolbar／safe area 可以增加實際最小高度。關閉／最小化釋放 scheduler session，flexible Color.clear 不把 placeholder 固定成新尺寸；重新開啟恢復使用者尺寸。View identity 跟隨 session，而不是每次導向產生 UUID，避免同一 session 的新旧 model demand 相互清除。Window accessor 由 viewDidMoveToWindow 綁定真正的 scene window；替換 window observers 前必須移除 NotificationCenter tokens。Preview 只出現在 Menu bar／Dashboard，收合時 unmount；Settings 不寫入 dashboard restart snapshot、不保留 popover transient state，也不增加 duplicate polling。

Xcode 27 的 SwiftPM `swiftbuild` 路徑在本次 runtime 對照中把 executable 的 `LC_BUILD_VERSION.sdk` 記成 14.0，使系統採用舊外觀；native build engine 則保留 SDK 27.0 與 deployment target 14.0。Makefile、bundle script 與 CI 使用 `--build-system native`；這是已 deprecated 的相容性措施，升級工具鏈時需重新檢查 linked SDK，不能只確認編譯使用了 SDK 27。Apple 的 [Liquid Glass adoption](https://developer.apple.com/documentation/technologyoverviews/adopting-liquid-glass)、[materials](https://developer.apple.com/design/human-interface-guidelines/materials) 與 [sidebars](https://developer.apple.com/design/human-interface-guidelines/sidebars) 是採用依據。

## Dashboard shell

Dashboard 保持 narrow popover，不做 desktop sidebar。WHO 與 WHAT 是兩個視覺上可區分的軸：client axis 是 capsule pills 橫排（[`DashboardTabs`](../../Sources/Syrtis/Views/DashboardTabs.swift)），只有 selected pill 提供真正 glass selection surface；lens axis 是單一 Liquid Glass navigation surface 加 quiet sliding thumb（[`ViewSwitch`](../../Sources/Syrtis/Views/ViewSwitch.swift)），不以 content card fill 假冒 glass track，也不疊第二層 glass thumb。Header／navigation 維持在 scrolling content 之上；26+ 的 content scroll edge 使用系統 `.soft` effect。八個 lens 在 400pt 下仍全部可達：native horizontal overflow 並把 selection scroll into view，不用 minimumScaleFactor 擠壓。Order、raw values、bindings、selection memory、routing、keyboard modifiers、resizable height 全部沿用既有；Agent limits 留在 Overview；user 的 card order／hiding 不變；native status error／loading／stale 保持可區分。

## Lens inventory

`AppView` 的八個 lens 全部在 scope 內，缺一不可：overview、quota、models、monthly、daily、hourly、stats、agents。Stats 是正式 lens，不是附屬。另有 global Overview 上的 onboarding setup cards（含 animation pace card）、Quota lens 上的 attribution onboarding card，以及 `--icon-gallery` 的 agent icon gallery——gallery 是 debug-only screenshot aid，不持久化任何 state，不是 shipped feature。

## Typography and controls

正文 13、細節 12；沒有 uppercase tertiary section labels。Settings 改採原生 grouped `Form` 與 `Section`，不做自製大型深色卡片與 per-row glass。控制項全數使用 platform native controls：選單統一使用原生 labeled menu `Picker`，不使用自製 checkmark rows；開關使用原生 switch Toggle。單一 group box 內不混用第二套選擇器慣例。Persisted value 不在選項中時（如 Discord binding 的 malformed sentinel），Picker 顯示 disabled unavailable row 誠實呈現，不代寫合法值。所有 label 都是完整句子，可整句翻譯；識別碼（client／model 名、window label）只在顯示端查表。圖示選取維持輕量原生選取邊框，不做過度裝飾的 per-icon glass/cards。

## Surfaces and glass

Liquid Glass 是浮在內容之上的 navigation／control layer，不是每張資料卡的背景。Settings sidebar 採用系統原生 `NavigationSplitView` 與 native sidebar split material，移除過時的 `sidebarGlass()` helper。Custom dashboard navigation 使用真正的 `.glassEffect(.regular.interactive(), in: .rect(cornerRadius: cornerRadius))`；[`GlassBackground.swift`](../../Sources/Syrtis/GlassBackground.swift) 的 `navigationGlass()` 提供這個 effect 與 macOS 14–25 的 material fallback，reduce transparency／increase contrast 時改用不透明 semantic surface。不要以 quiet rounded fill 或一般 blur 宣稱 Liquid Glass。

`glassCard()`（[`Cards.swift`](../../Sources/Syrtis/Views/Cards.swift) 的 `GlassCardBackground`）保留 quiet adaptive content fill：panel 內 controlBackground 0.72、外面 0.85，reduce transparency 或 increase contrast 下轉不透明。Chart hover tooltips 共用 `tooltipSurface()`，即同一 content surface；不疊第二層 glass。內容卡片的節制不能替代 sidebar、toolbar 與 navigation 的真正 Liquid Glass adoption。

官方設計資源與準則：
- [Apple Design Resources macOS 27](https://developer.apple.com/design/resources/)
- [SwiftUI Updates](https://developer.apple.com/documentation/updates/swiftui)
- [Human Interface Guidelines: Materials](https://developer.apple.com/design/human-interface-guidelines/materials)
- [Adopting Liquid Glass](https://developer.apple.com/documentation/technologyoverviews/adopting-liquid-glass)
- [Applying Liquid Glass to custom views](https://developer.apple.com/documentation/swiftui/applying-liquid-glass-to-custom-views)

各平台 composite 與 runtime 外觀須以真實環境渲染為準，不作未經證實的視覺通過宣稱。

> 歷史上的 nested `.clear` card glass recipe 見 [`history/liquid-glass-experiments.md`](history/liquid-glass-experiments.md#shipping-recipe)；本 contract 取代該 recipe 為現行實作，不改寫歷史。

Motion、transparency、increase contrast 一律走 semantic environment（`accessibilityReduceMotion`、`accessibilityReduceTransparency`、`colorSchemeContrast`）。Panel 內的 selection animation、chart regrow、heatmap crossfade 只在 panel 內啟用；panel 外不掛 `.animation(nil)`（那會吃掉 popover 自己的 tab crossfade）。

## Motion and lifecycle

不新增任何 timer 或 poll；animation 只在可見時執行；不做 process-wide image cache；list row 不掛 heavy effect。Closing／minimizing 釋放 live content demand，與改版前完全一致。Scheduler／model／engine 只有在證明為 UI lifecycle bug 且經明確核准時才動。

## Preserved semantics

本 contract 不擁有、也不改變：所有 preference keys and values、quota-only provider（如 Grok Bot 與 Build 分離）、Claude 額外帳號的獨立身分、quota-only provider 無本機用量時的明確空狀態、usage attribution 三態（assigned／excluded／unassigned）各自可見且相加等於該 client 本機總量、lifetime 與 last-good／stale 區分。文案不得宣稱扣抵。

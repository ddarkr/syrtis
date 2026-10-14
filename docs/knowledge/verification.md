---
status: active
id: kb-verification
kind: canonical
scope: repository
read_when: changing runtime code, running a local build or UX acceptance, parser output, cache behavior, FFI contracts, or this knowledge tree
last_verified: 2026-10-15
sources: [".github/workflows/ci.yml", ".github/workflows/ci-release.yml", "Makefile", "Package.swift", "scripts/bundle.sh", "Sources/Syrtis/ClientTray.swift", "Sources/Syrtis/StatusItemController.swift", "Sources/Syrtis/MenuBarTextColor.swift", "Sources/Syrtis/Views/AgentIconView.swift", "Sources/Syrtis/Views/SettingsPanel.swift", "Sources/Syrtis/SelfTest.swift", "Sources/CrossCheckHarness/main.swift", "Sources/TokenBarCore/AgentUsage.swift", "Sources/Syrtis/ClaudeExtraRoots.swift", "crates/tb_core_ffi/src/agent_account_scope.rs", "crates/tb_core_ffi/src/agent_quota_history.rs", "crates/tb_core_ffi/src/agent_kiro.rs", "crates/tb_core_ffi/src/kiro_integrations.rs", "crates/tb_core_ffi/src/extra_scan_paths.rs", "docs/knowledge/plans/provider-quota-pace.md", "docs/knowledge/plans/codex-historical-pace-v2.md", "public TokenBar-Windows PR #7", "public Syrtis PR #114", "public TokenBar-Windows PR #12", "AGENTS.md", "memory-derived hermetic verification practice", "memory-derived local build indexing incident"]
---

# Verification contract

## 文件目的

這份文件把 Syrtis 的驗證分成 deterministic fixture、跨語言契約、runtime smoke、cache invalidation 與 repository hygiene。目標不是堆命令，而是讓每個修正都證明「舊行為會失敗、新行為正確、常見資料不回歸」。

## 目錄

- [Evidence model](#evidence-model)
- [Hermetic fixtures](#hermetic-fixtures)
- [Runtime and FFI gates](#runtime-and-ffi-gates)
- [Local build and UX acceptance](#local-build-and-ux-acceptance)
- [Cache and sibling checks](#cache-and-sibling-checks)
- [Cross-language invariants](#cross-language-invariants)
- [Documentation checks](#documentation-checks)
- [Failure interpretation](#failure-interpretation)

---

## Evidence model

| Evidence layer | Answers | Cannot prove alone |
|---|---|---|
| Hermetic fixture | 觸發條件下 old/new 是否分歧、修正是否收斂 | 真實 GUI lifecycle 或 provider 網路狀態 |
| Unit or core test | 純函式、parser、fold、schema contract 是否穩定 | Swift/AppKit integration |
| FFI smoke | Rust -> C ABI -> Swift decoder 是否能端到端運作 | 所有特殊資料條件的正確數字 |
| Live app check | 真實 session、視窗 lifecycle、外觀與更新流程是否不崩 | 沒有觸發資料時的 correctness fix |
| CI | 可重複的 build/selftest/smoke gate | 本機 private data 與人工 UX 判斷 |

Calendar Bars rollover 的獨立 native smoke 使用正式 `UsageChartCard` 與停在昨天的 synthetic graph，All years／目前年度均在 own-process compositor 顯示今天為右端、今天空 bar。UI-free before／after smoke 同時證明首日移出後 token／cost 不再計入。這是日期視窗與實際 chart 的短程證據，不代表安裝版在真實午夜的長時間 lifecycle 已驗收；暫時的 native probe 不保留於 app。

## Hermetic fixtures

當修正效果取決於本機可能沒有的 session、duplicate key、cursor、WAL、sibling metadata 或 provider cost 時，優先建立合成 fixture。測試應同時保留 old-fail/new-pass 證據，並另加無觸發條件的保值 case。

> **Hermetic 原則：** Live app 在沒有觸發條件時顯示「沒有變化」，只證明常見資料不崩，不能證明修正有效。權威證據是可重跑、與本機資料無關的 fixture。

PT0 的 hermetic authorities are Rust last-good and binding decisions, refresh status-before-body ordering, Grok monthly additive behavior, Copilot loader precedence, Antigravity precedence, and the Kiro multi-source token loader (kiro-cli SQLite and Kiro IDE token file) with its last-good caching; the FFI A/B publication-generation ordering and Swift diagnostic-candidate plus isolated UserDefaults scalar and local publication-state tests are required at the cross-language seam. The Rust fixture must pause A after its gate helper returns, let B obtain generation 2 and record its return first, then release A; it checks both return order and payload generations/content. A separate exhaustion fixture starts at `u64::MAX - 1` and proves the next call fails closed without invoking the publication body or repeating a generation. Swift fixtures distinguish bridge failure from malformed/missing successful data, prove Settings identities change across generations, legacy resolved values, selections, and exclusions, drive the tray apply seam with generation 2 terminal followed by generation 1 success to prove the late result resolves to generation 2 and cannot revive its scalar, and prove a generation 3 Dashboard publication replaces both an older tray payload and scalar before the tray's own poll returns while changing the gauge signature that gates immediate rendering. Live smoke requires authorization for that run and cannot replace these fixtures.

| Fixture property | Required assertion |
|---|---|
| Duplicate or replay | 舊路徑的 total 與對照路徑分歧；新路徑與對照收斂 |
| Sibling-only write | 預設 fingerprint 不失效；完整 fingerprint、mtime probe、prune 都失效 |
| Provider cost | 缺失成本可估算；明確 provider-reported 成本不可被 stale pricing 覆蓋 |
| Hidden client | non-empty partial selection 在 Rust fold 前排除未選 client；`nil`／empty clients 依 C ABI contract 代表 all clients；all-hidden 由 Swift lens strict membership 阻擋；對外發布面另以 hermetic payload-builder fixture 證明隱藏 client 的 token／cost 與 top-client 標籤都不出現在送出的 payload，且未註冊 id 在 producer 端就被正向 allowlist 濾掉——既不進數字也不進標籤，因此中性常數自 `payload()` 已不可達，改以「整張圖只有未註冊 id 時發布 nothing」證明其為死路 |
| Quota account scope | 以 temporary application-data root 驗證 exact 32-byte key、每次 reload、single winner、random／length rejection、key-loss orphan recovery、one-poll deferral、atomic pre／post-commit outcome、authenticated metadata MAC、valid payload＋valid MAC 的 wrong schema、valid-MAC semantic conflict、quarantine collision／identity-bound rollback與 raw-value privacy；macOS／Unix另驗證 directory `0700`／file `0600`、final symlink／non-regular／mode／inode swap fail-closed，以及 final directory 拒絕但 trusted root 下 symlinked ancestor 允許。Windows 端的 account-scope fixture 與低階 secure-storage mechanics 自 #276 起只存在於 Syrtis-Windows 自己的 `crates/tb_core_ffi`（[Syrtis-Windows#147](https://github.com/Nanako0129/Syrtis-Windows/issues/147)），本 repo 不再保留。不得呼叫真實 Keychain 或 provider credential |
| Quota history | Reset jitter、floating zero、duration lifecycle、partial／future-reset cycles、early／irregular reset 的 observed-duration close、supersede 門檻（28 小時以下的窗須逐 gap 證明與量子時代**完全相同**，不得只釘邊界；週窗須證明 20 分鐘漂移不關窗而 3 小時關窗；`successor_duration_from_series` 的測試兩個候選 group 必須帶**不同** duration，且低鍵那組帶**長** duration，否則放寬判準的變異無法被觀察）、切短 cycle 不進擬合（須同時斷言 `complete_cycles` 差 1 與 pace 相同，斷言在回傳值上；`advertised_nominal_duration` 的 `None` 分支須以 duration 異質的全 Observed series 覆蓋）、upgrade repair（他系列交易仍 restamp 殘留 group；舊點已 prune 則不發明 cycle）、active-series capacity、account isolation、corrupt recovery與current-actual shift都以temporary v3-family store（schema 4）驗證；Codex schema-2只驗current-account-only、byte-exact read-only migration，live provider refresh只作smoke |
| Quota history identity | 三次連續的 credential marker 變更必須產生**同一個** `SeriesKey`，且同一組 marker 仍必須產生**三個相異**的 `ProviderCacheBinding` 與三個相異的 plan-cache key——兩者要一起斷言，否則證明不了「拆身分」而只證明了「換了個鍵」。另需：`scope-history-v1` 的 known vector（期望值必須在 crate 外獨立算出，不得取自實作）與對 `scope-id-v1`／`scope-lineage-v1` 的 domain separation；常數路徑不得建立 metadata 檔；`accountScope` 為 `Err` 但 `historyScope` 為 `Ok` 的快照必須寫入**零**筆 history；每一條 authoritative 路線各有 byte-equality fixture。禁止在 real store 上跑 before/after——那會改寫使用者的生產歷史庫 |
| Overflow input | old arithmetic fails or wraps in the targeted site；new saturating path remains bounded |
| Individual client tray | UI-free SelfTest鎖定兩個defaults的parse前byte cap、entry／ID cap、deterministic serialization與超限no-writeback；client-scoped Auto／explicit／missing、error-only quota provider仍可配置、`antigravity-cli`只在quota lookup映射到`antigravity`且identity保持獨立、main完整route與per-client lens記憶互不污染、Settings row／picker狀態、official icon 1x＋2x reps、newer accepted publication與visible／AX／tooltip privacy都由synthetic graph／quota payload驗證，不建立真實system status items |
| Usage attribution | 政策以結構性斷言驗證而非逐條列舉：每個 bound provider 必須指名 `providerOwnClient` 保護對象、每個 first-party 廠商必須能從其 opencode 標籤解析回自己的 client、表內每個 client 必須在註冊表中。另驗證：來源 client 可為註冊表外的動態 id 而目標不可、自家訂閱優先且不需要 quota snapshot、未調查的來源回傳 nil 而非斷言 API 支出、已宣告的 router 計入它簽入的訂閱 |
| Quota curve snapshot | Binding admission 以真實 snapshots 驗證（trusted scope 不足以綁定：帶 `error` 的 last-good 與帶 `transport_diagnostic` 的 degraded 都必須排除），window key 由 production mapper 產生而非寫進 fixture，因此 mapper 端改身分會失敗而不是靜默 unbind；lifecycle 驗證 serialization 失敗保留前一個 tuple、generation 過期為錯誤、process restart 後不供應；binding lock 必須在 history I/O 前釋放，且 read 之後重新解析 binding——tuple 在 I/O 期間被替換（含同 generation 換帳號）必須 fail closed，settled binding 則仍正常供應 |
| Cache schema | 舊版本 cache 不被當成新 layout 靜默接受；新 layout 可重建並 reload |
| Durable usage migration | Validation and backup keep the same WAL read snapshot while a second connection commits between backup steps. Source-free upstream format-10 Claude fixtures preserve exact tokens, 1h-write/reasoning lane order, timestamps, turns, retained membership, and byte-identical originals; repeated preparation is idempotent, and unsupported newer retaining layouts cannot activate a target. |
| Provider transport fallback | last-good binding、refresh status-before-body、terminal/absent/4xx/schema/required-meter clearing、Grok additive monthly、Copilot loader、OpenCode Go loader／`usable_success("opencode")` last-good／sibling-isolated window decode、以及 diagnostic allowlist 都以 hermetic responses 驗證。Provider 註冊一致性（#324）：Rust `the_provider_table_is_todays_card_order` 釘住表格順序（一致性檢查都走訪表格，所以「從表中移除一個 provider」只有它能對每個 provider 都抓到；codex、kiro、opencode 另有既有 selftest 直接點名）、`every_provider_in_the_table_has_a_usable_success_rule`（含表外 id 的 control）、`fetch_in_order` 三個 stub 測試（完成順序≠表格順序、空結果與多帳號、互等 stub 證明並行）；Swift selftest 以 `tb_quota_provider_ids` 的回傳逐一檢查 ClientRegistry、官方 icon、demo 卡片與 transport 歸屬，並有「清單非空且含 codex」與「表外 id 記為 unknown」兩個 control。Rust 部分由 CI 的 `cargo test -p tb_core_ffi --locked` 執行（動到 Rust 的 PR 與每次 main push），Swift 部分由 selftest 執行。`ProviderId` 是 enum。以下都在非測試建置實測為編譯失敗：在 fetch 裡寫 `ProviderId::Test(..)`（E0599；測試建置下會通過，因為 `Test` 只存在於 `cfg(test)`）、`usable_success` 漏一個分支（E0004）、把 id 改成字串（E0308）。`the_caller_id_is_stamped_on_a_successful_snapshot` 以另一個 id 建立 fixture（呼叫前先斷言它帶著那個 id），經 apply 後發出的是呼叫端的 id、last-good 存在該 id 的 slot；拿掉蓋章那行即紅（先失敗的是發出 id 的斷言；把它拿掉，slot 斷言也會獨立失敗，因為 `usable_success` 對 fixture id 回 false、快取被清除）。「卡片以其資料來源 provider 的 id 發出」只有部分受測：`each_fetch_is_handed_its_own_entry_id` 釘住 `fetch_table` 把每列自己的 id 交給該列的 fetch（改成交別的 id 即紅），蓋章測試釘住 Success 分支。表格配對、fetch 是否把 id 傳下去，以及錯誤卡片、last-good fallback、`enrich` 等其他可能改動 id 的地方都沒有專門檢查，靠審查 |
| Grok Bot adapter | Synthetic desktop／SQLite stores 驗證 active-account precedence、解碼失敗不得跨帳號回退、signed-out 不復活 legacy login、team header 與 endpoint 配對；quota mapper 驗證 stable weekly key、provider duration、無效 meter、pooled allowance，以及 camelCase／snake_case 的明確無配額狀態；保留有效零用量與省略 optional flag 的對照案例。使用 temporary scope backend 驗證 credential rotation／帳號／team 的 cache 與 history 分離，並由 production outcome adapter 驗證同 request transient 才可沿用 last-good，terminal／absent／schema error 清除。Swift selftest 驗證分組成員、獨立額度開關、實際卡片 visibility filter 與 Auto selection 一致；另由正式 quota poll 和 curve reader 驅動 Bot-only fixture，沒有 graph／本機用量仍須載入 sparkline、summary 與 heatmap，並驗證 Build／Bot 分離、隱藏後清除、publication generation 切換與無配額不得沿用歷史；不得在 unit tests 使用真實 Keychain 或 provider credential。**Keychain 事前同意**另以五個 adapter case 驗證：加密 fixture 在同意缺席時必須回傳 marker error 且 decrypt closure 從未被呼叫（closure 本身是 `unreachable!()`），配一個計數 closure 的 control 證明同一份 fixture 在同意存在時確實走到 decrypt——沒有這個 control，只要 fixture 因為 key 名稱或 active account 寫錯而根本到不了 decrypt，否定式斷言就會安靜通過；同意缺席且存在 desktop 登入時不得回退到 Cursor 憑證（同樣配一個「沒有 desktop 登入時 Cursor fixture 確實載入」的 control）；`plaintext:v1:` secret 在同意缺席時仍須載入，證明 gate 釘在 decrypt 而不是「有登入」；純 Cursor 使用者完全不受影響。`agent_usage` 驗證 marker 映成 `source == "keychain-consent"`、其餘 terminal 與成功都仍是 `"oauth"`（否則測試可由「全部映成新 marker」通過），且該 source 會進入實際發布的 snapshot。Swift selftest 驗證 placeholder source 不進 `configuredClientIds`（配一個「一般 error 卡片仍可導覽」的 control）、consent 列仍留在限額卡片、儲存的答案為三值（`nil`／`false`／`true`，不可塌成 `bool(forKey:)`），以及授權會送出精確的 `{"grok-bot":true}` 並喚醒輪詢迴圈、拒絕則兩者皆無。變異證據：刪掉 decrypt 前的 guard 使兩個 adapter case 轉紅；把 control 的同意翻成 false 使 control 轉紅 |
| FFI publication | Provider run、JSON serialize、envelope、raw C-pointer publication 的 single-flight、gate-assigned checked `publicationGeneration`、exhaustion fail-closed、可反轉的 C return order，以及單次 run 內 provider 並行分別驗證 |
| Source-aware filter parity | `tb_filter_parity_probe` uses one context, fresh graph, `token0…token5`, exact integer comparison, diagnostic-only cost deltas because pricing refreshes independently from source generations, and short-circuits invalid later scans. Its synchronous callback seam tests stable match/mismatch, price-only refresh, tokenUnavailable versus graph failure, every source-change boundary, Agents-only late changes, call ordering, and a size-changing append token. The dedicated vendor fixture covers canonical, cc-mirror exact gating, synthetic gateway, duplicate canonical paths, inherited scanner-root isolation, unattributed `Main`, and cold/warm cache parity. |
| Phase 1 source lane | 以可控制的排隊 fixture 證明 aged graph 等待期間被另一工作更新後只 compute 一次；forced graph 等舊 scan 完成後必須獨立讀到 request 後的新輸入，而非 join／cache settle。Extra roots／Claude exclusions 在排隊與 scan 中變更時，分別驗證新 scope capture 與舊 generation publication rejection，並保留 current-generation 成功的 control。 |
| Recent visitor parity | Pending engine 的 recent path 與 materialized oracle 在相同時間窗逐則比對 identity、timestamp、token lanes、provider／model／trace 與 cost semantics；涵蓋 Synthetic gateway、alias、empty ID 與 dedup／authority，不以只有總量相同代替 exact parity。 |
| Real WAL mutation | 使用真實 SQLite WAL transaction 做 update／delete／insert，明確斷言 main DB bytes、mtime、size 不變而 graph／report／tail 更新；另驗 cold／warm、NULL／empty exclusions、巨大無關 content 不進 projection，以及原始 typed row 無效時仍按原契約拒絕。 |
| Tray demand and cancellation | UI-free acceptance 鎖定 rate 的唯一 owner、static sand／動畫／tokens-per-min predicates、graph 的 title／visible client items／Discord predicates、重新有 demand 立即 fetch、last-good 保留，以及取消／replacement 後完成的 payload 不發布。獨立 30 秒 quota-staleness render 在無 graph／rate demand 時仍運作；真實 native UI lifecycle 另驗，不由 predicate tests 代替。 |
| Compact quota wire | Rust folds 與 Swift attribution fixtures 對 exact half-open windows／cycle、declared account/model scope、assigned／excluded／unassigned、undated／turn counts、coverage、last-good 與 coherent pricing generation 驗證；實際 Swift → C ABI → Rust smoke 解碼 compact response 並釋放每個 native pointer，不以 mock echo 取代 contract。 |
| Demand scheduler | 隔離 bundled selftest 驗證共享 ingestion、exact producer scopes、30 秒 active／5 分鐘 idle verification、dirty debounce、manual postrequest barrier、clock-only expiry 可越過被阻塞的 source verification、scope replacement／final close 的 late publication rejection、ordinary-query／quota pins、pricing expiry、midnight／timezone 與 lazy expanded-model demand。首張 graph 在 blocked union capture 前發布；side capture 借用 installed token 時不提早釋放，root replacement 與 query completion 後僅釋放一次。 |
| Calendar bar rollover | 同一份 payload 在午夜前後，All years／目前年度的 Model 與 Agent bars 都前移一個日曆日；今天無 metric activity 時為空 bar，移出的首日不再計入 token／price。另驗 message-only、empty payload、跨年與其他年度的 selected-client metric anchor，不把 scheduler query 次數當成可見日期範圍正確的證據。 |
| Native lease admission | 實際 sync／query／window ABI 驗證省略 primary quota 的 `clientId`、extra-account validation、same-producer aliases、64-scope／32-producer admission、共享 metadata 容量與超過 32 個 quota aliases 的 token 可用性。重送 normalized root registry 保持 lease 可讀，effective change 才使舊 lease 過期。 |
| Trusted report folds | Graph／models／hourly／Agents 與 buffered oracle 比較 exact integer、cost 與 traversal output；malformed later row 不發布 partial aggregate，public visitor 保留 failure-before-first-callback、chronological order 與 reentrant source replacement。 |
| LocalClients lane reuse | Unchanged OpenCode＋changed other-lane 的 totals／ordered public output、source-order reversal、SQLite／JSON／path／provider changes、empty transitions 與 superseded handle rejection。GC 後 copied references 仍可解碼，retired decision/index rows 不累積。 |
| Stale query completion | Paused ordinary query 的 failure 不得失敗新 owner 或已安裝的新 lease。Multi-graph side batch 在首個 await 期間 root／owner 改變後，後續 query 仍使用 capture 前的 epoch 與逐-query owner identity；於下一次 sync 被阻塞時斷言沒有舊結果，再驗證新 capture 可發布。 |
| Native source hints | Fixture 驗證 missing-root recovery、scope-exact siblings、owned sidecar／temporary basename grammar、WAL、invalid UTF-8 fallback、native byte identity、lost events、bounded coalescing 與 stopped／replaced callbacks；實際 app 開關 demand 另驗 FSEvents callback／retain／release 的非 MainActor execution，不以 filter tests 代替 native stream lifecycle。 |
| Extra scan roots | `crates/tb_core_ffi/src/extra_scan_paths.rs` builds a real `LocalSourceContext` pointed at a temp `home_dir` and writes real fixture `.jsonl` files under a second temp root, then calls `set_from_json` and re-derives `report_options`/`parse_options` — no mock scanner. Because Claude's dedup key (`{message.id}:{requestId}`) does not include the source root, every fixture states which content relationship it exercises: id-disjoint (extra root sums with the primary), duplicate dedup key across two roots (total unchanged), and the same path registered twice (canonical dedup, unchanged). Coverage: adding an id-disjoint extra root increases the total and removing/never-calling the setter reproduces pre-feature behavior exactly (`scanner_settings.extra_scan_paths` equals `ScannerSettings::default()`'s empty map); mutating the extra fixture's token count is demonstrated red-then-green by reverting the `report_options`/`parse_options` wiring and rerunning the suite, which turns 7 of the 12 cases red while exactly the five that do not depend on the wiring stay green (duplicate dedup key, malformed JSON, file-as-root, never-call, and unsupported client id) — that boundary is the expected one; a path is registered whenever it can still become a readable directory, so an absent root stays in the registry and a later scan picks it up with no second setter call (the regression creates the directory and its fixture after registering and asserts the total moves 0 → 100), while a path that can never be a scan root — empty, relative, or an existing non-directory — is rejected and kept out, with the file-as-root case using a real parseable transcript so that a regression would show up as its contents entering the totals rather than as a shape mismatch; an unsupported client id is rejected before any path is counted; none of these panic or block the rest of the registry; malformed JSON returns a structured error and leaves the registry untouched; a warm `tb_graph` cache cannot hide a newly registered root, by two separate mechanisms that are tested separately — `local_source_change_token` moves once the root's files enter the scan set, which covers a cache entry that has aged past its window, and `lib.rs`'s `setting_extra_scan_paths_drops_the_caches_that_could_answer_from_the_old_roots` covers the window itself by seeding `GRAPH_CACHE` and the tail stamp, driving the real `extern "C"` setter, and asserting both are gone; that test drives the FFI entry point rather than `set_from_json` because the invalidation hangs off the wrapper, and deleting the `invalidate_scan_caches()` call turns it red; `a_scan_that_started_before_a_root_change_does_not_publish_its_result` covers the third window, an in-flight scan publishing after the clear, by taking the generation as a scan would, moving it through the real setter rather than by bumping the counter directly, and asserting both `publish_graph` and `stamp_tick_if_current` refuse — with a same-generation control so a guard that refused everything could not pass; its three mutation targets (each guard's generation check, and the bump itself) were each removed and each turned it red; and a config dir's `projects` and `transcripts` sub-roots both contribute (id-disjoint), proving neither sub-path is silently dropped. `Sources/Syrtis/SelfTest.swift`'s Claude-extra-scan-roots section is Swift-only and does not call the FFI (the live process re-reads `HOME` from the environment, so Swift cannot point it at a temp root); it covers `ClaudeExtraRoots.expand`'s two-sub-root output, `payloadJSON`'s exact wire shape (one dir, multiple dirs, an explicit empty array on `[]` rather than an absent key, and dedup of a repeated dir), and the `isRejectedRoot`/`isMissing` path-sanity predicates. |

> **Quota account-scope evidence boundary：** Legacy development Keychain path 仍被架構禁止。本次 test refactor 的 production-prefix byte freeze 只證明這次沒有新增或修改 production bytes；compiler／build 只能證明 module 被編譯並符合型別與連結契約，不能自動證明未來不會重新引入 Keychain query。Source／Cargo spelling scan 不是 behavioral guard，因此不列入此 fixture contract。

> Quota-history schema 4 migration：v3 → v4 是就地、惰性的升級，驗收全部釘在「能失敗」上。`a_v3_store_upgrades_in_memory_keeps_every_sample_and_converts_on_disk_only_when_written` 把三條宣稱綁在同一個函式裡，因為順序是隱形的前提：v3 store 若被 quarantine 而非升級，store 會是空的，「每筆樣本的 plan 都是 None」在空集合上恆真。它先斷言樣本數（5 筆、2 條 series，且刻意跨 phase bucket——同桶樣本會互相取代，否則 fixture 會比讀起來小得多），再斷言載入後未 quarantine、schema 為 4、且與升級前的 store 完全相等；接著重讀磁碟，斷言檔案仍是 `schemaVersion: 3` 且樣本仍無 `plan` 鍵（惰性語意本身，若只斷言記憶體中的版本，惰性與積極兩種實作都會綠）；最後跑一次真的會寫檔的交易，才斷言磁碟轉為 4、樣本數 N+1、且全部 `plan` 為 `None`。fixture 由真實寫入路徑產生後再降級成 v3 形狀，並斷言 `plan` 鍵確實存在才移除——否則某天停止序列化該欄位會讓這個測試悄悄退化成 v4 讀 v4。V5（`#[ignore]`、由 `TOKENBAR_QUOTA_STORE_COPY_SOURCE` 指向操作者自行複製的真實 store 副本）連同另外三個同型的 operator-copy 測試已於 2026-09-07 移除（#274）：它們從不在 CI 執行，hermetic 的 `a_v3_store_upgrades_in_memory_keeps_every_sample_and_converts_on_disk_only_when_written` 才是 gate。結尾的位元相等只能證明「本測試沒有改動來源」，對複製與斷言之間的 panic 無效、對行程被砍完全無效——不開啟實體檔案才是唯一能挺過那些的保證。本機以副本實測 31 series／2835 樣本全數保留。`a_v3_reader_cannot_parse_a_v4_store` 用一份 v3 形狀的鏡射結構把降級的不可逆性變成可證偽的斷言，而非散文。變異驗證逐一紅轉綠：拿掉升級分支（hermetic 與真實資料各紅一次，後者的失敗訊息正是使用者會遇到的 quarantine）、把升級改成載入即寫檔、讓寫入路徑塞入非 `None` 的 plan、拿掉鏡射結構的 `deny_unknown_fields`、給新欄位加上 `skip_serializing_if`。
>
> Historical runtime retirement checkpoint（2026-07-31）：`agent_history.rs` 的 v2 writer／evaluator 與專用 evaluator tests 已刪除；現行 gate 是 `agent_quota_history.rs` 的 **schema-4** evaluator 加上 current-account-only schema-2 importer regressions（檔名中的 `v3` 是 store 家族代號,與 schema 版本無關——見上一段的遷移）。舊 Historical pace v2 cross-port checkpoint 仍保留於下方，僅作歷史 parity 證據，不是 current gate。

> Load-time repair persistence（#207）：`a_load_time_clock_repair_reaches_disk_once` 與 `a_load_time_sample_drop_reaches_disk_once` 各跑兩次 body 什麼都不改的交易，斷言第一次 `save_call_count` 為 1、寫出的檔案不再需要修復，第二次仍為 1（修好的檔案不重寫）；兩者都先斷言 fixture 在磁碟上確實需要修復，否則「寫了一次」可能只是別的原因。`a_v3_stamp_alone_is_not_written_by_a_no_op_transaction` 釘住惰性升級不受影響：只有 v3 版本號的檔案，同樣的交易 0 次寫檔、位元組不變。存檔失敗時，若 body 本身沒有變更，`a_failed_repair_write_back_does_not_fail_the_transaction` 斷言交易仍回 `Ok`（寫回修復只是 housekeeping），並以 body 有變更時仍回 `AtomicSave` 當 control。v3 檔若同時需要修復，會隨這次寫回一併轉為 v4，與任何其他有寫入理由的交易相同。`a_future_sample_drop_reaches_disk_once_and_keeps_the_series` 釘住時鐘修復只丟樣本：一條 series 同時有上限以前與超過上限的讀數，no-op 交易寫檔一次，磁碟上 series 仍在、只剩上限以前那筆，第二次不寫；`future_sample_evidence_drops_only_that_sample` 在載入端斷言同一件事並確認兄弟 series 不受影響。變異驗證（2026-09-27 實測）：把樣本過濾改回整條 series 刪除、或完全不過濾（兩個 future-sample 測試紅）；交易忽略 `repaired`（時鐘、樣本丟棄、future-sample 寫回三者紅）；樣本丟棄不設旗標（樣本丟棄那個紅）；時鐘修復不設旗標（時鐘與 future-sample 寫回兩者紅）；旗標恆為真（時鐘、樣本丟棄、future-sample 寫回、v3 四者紅）；存檔失敗一律回錯（存檔失敗那個紅）；過濾邊界改成 `< upper_bound` 或 `<= observation_now`（future-sample 寫回紅，它有一筆剛好在上限、一筆落在 `(observation_now, upper_bound]`，並以 control 斷言四筆讀數各在不同 phase bucket）。`repair_is_a_fixed_point` 的 fixture 也含一筆超前樣本，確保冪等性是在新分支上量的。

## Runtime and FFI gates

The current CI runtime sources are [`.github/workflows/ci.yml`](../../.github/workflows/ci.yml) (every PR and every main push) and [`.github/workflows/ci-release.yml`](../../.github/workflows/ci-release.yml) (main pushes only: the release-configuration build and the bundled selftest, beside `ci.yml`). CI builds the Rust static library (on a pull request whose Rust inputs match an entry main saved, it links that library instead of rebuilding — the key and its inputs are in the workflow; pushes to main always build it), builds Swift, runs the core selftest, and runs the FFI smoke binary. Both `ci.yml` and `release.yml` run on GitHub's `xcode-27` image, and a local build needs Xcode 27 too: the macOS 27+ glass panel is built on an API only the macOS 27 SDK has (see [`history/liquid-glass-experiments.md`](history/liquid-glass-experiments.md#macos-27-glass-panel)). Those are CI build and smoke checks, not the complete local code-change gate. The local build order comes from [`Makefile`](../../Makefile) and the linker contract comes from [`Package.swift`](../../Package.swift).

```bash
cargo build --release
swift build
make selftest          # = swift run Syrtis --selftest -AppleLanguages "(en)"
swift run Syrtis --smoke
```

每一項都跑 debug configuration 的 bare executable，`Bundle.main.bundleIdentifier` 因此是 nil。凡是以那個差異為條件的值，在 suite 看得到的地方是一種樣子、在出貨的地方是另一種樣子，而任何 source scan 都關不掉這個缺口（#146 寫過三道、三道都被繞過，缺口不在原始碼文字裡）。CI 因此在 push 到 main 時多跑一次 `make selftest-bundled`：release build、裝進 `.app`、從 bundled binary 執行同一套 suite。

bundle identity 是這個 target 唯一的旋鈕，而它在兩個危害之間取捨。bundled run 會把 `UserDefaults.standard` 解析到 identifier 指名的 domain，而 suite 確實會寫進去（`PopoverChrome.heightKey` 與 dashboard year key 都是先寫再還原，因為生產型別直接讀 `.standard`）；用出貨 identifier 就會寫進安裝版自己的偏好設定。

所以本機預設是拋棄式的 `com.nyanako.tokenbar.selftest`，而**那是比較弱的 gate**：它抓得到以「identifier 為 nil」為條件的值，抓不到以出貨字串本身為條件的值——後者在本機走安全分支，只有裝起來之後才走另一條。CI 用 `make selftest-bundled SELFTEST_BUNDLE_ID=` 補上，空值代表「`scripts/bundle.sh` 的預設」也就是出貨 identifier；runner 是拋棄式的、沒有安裝版可污染，開發者的 Mac 有。空值而非再寫一次字面值，是為了讓未來改名只有一處要動、不會把這道 gate 無聲地弱化成對不上的字串。

app 名稱**不是**第二個旋鈕。它一度是，而那等於把同一個逃脫換一個屬性重演一次：以 `CFBundleName == "Syrtis"` 或 bundle URL 結尾為條件的值，在叫別的名字的 gate 裡會走安全分支。所以它組出來的是貨真價實的 `Syrtis.app`，改用 `OUT_DIR=dist/selftest` 讓路，順便仍然不會覆蓋手動建的 `dist/Syrtis.app`。

與正式發版 bundle 仍然不同、且**不打算**逐輪 review 才發現的部分：安裝路徑（`dist/selftest/` 而非 `/Applications/`）、version 與 build number（用 `bundle.sh` 預設，正式值由 `release.yml` 傳入）、簽章（ad-hoc 而非 Developer ID）。以這三者為條件的值超出這道 gate 能觀察的範圍，任何本機組裝的 bundle 都關不掉，只有安裝 notarized build 才行。這裡**觀察得到**的是 identifier、名稱，以及 release configuration 本身。

**它不是 `make selftest` 的超集**：`#if DEBUG` 後面的斷言在 release configuration 不存在，所以 bundled run 的斷言數比較少。兩者都是 gate，互不取代。

### Local full code-change gates

For Rust or cross-language code changes, the local full gate adds formatting, the Rust test suite, the all-targets Clippy pass, and the repository build:

```bash
cargo fmt --all -- --check
cargo test
cargo clippy --workspace --all-targets
make build
make selftest          # = swift run Syrtis --selftest -AppleLanguages "(en)"
swift run Syrtis --smoke
```

`cargo test` and `cargo clippy --workspace --all-targets` are local full code-change gates. CI's Rust-build subset runs `cargo test -p tb_core_ffi --locked` and `cargo clippy -p tb_core_ffi --all-targets --locked --no-deps`; neither replaces workspace engine verification. The owned engine retains `#![deny(clippy::all)]` and the repository pins its Rust toolchain. A cached-library PR skips rebuilding unchanged Rust inputs.

Live account-scope smoke必須在hermetic security suite通過後才執行，且每次重新執行都需要當次明確授權。若出現任何Keychain或credential授權視窗，立即停止process並把smoke判為失敗；不得要求輸入登入密碼、讀取secret或用真實credential診斷。

| Gate | Expected evidence |
|---|---|
| Rust | Release static library builds from the current source |
| Swift | SwiftPM links against the freshly built library from repository root |
| Selftest | UI-free TokenBarCore assertions pass。部分斷言逐字比對英文 UI 文案，因此語系必須鎖定 `en`（用 `make selftest`，或自行帶 `-AppleLanguages "(en)"`）；在中文系統上直接跑 `swift run Syrtis --selftest` 會因 `Format` 輸出中文而假性失敗，入口會先印出提示 |
| Bundled selftest | 同一套 suite 從 `dist/selftest/Syrtis.app` 的 release binary 通過（`make selftest-bundled`），證明斷言看到的是出貨 configuration。CI 只在 push 到 main 時跑（`.github/workflows/ci-release.yml`，與 `ci.yml` 平行；打 tag 前兩個 run 都要綠），並以 `SELFTEST_BUNDLE_ID=` 帶出貨 identifier；本機預設拋棄式 identifier＝較弱版本。斷言數少於 debug run（`#if DEBUG` 的部分不存在），不是超集 |
| Smoke | Every C ABI entry point decodes or reports an intentional error envelope；account-scope path不得存取Keychain或顯示credential authorization UI |
| Account-scope storage | Hermetic security tests先證明permission、path、locking、atomicity與recovery；live smoke只驗證shipping data flow不彈授權UI，不取代fixture correctness |
| Windows secure storage | M19-B0證明Native candidate與核准Windows security/storage semantic source等價；M19-B1又在hosted Windows x64 runtime執行CNG、owner／exact protected DACL、final-component reparse、file identity、exclusive no-delete-share lock、replace、quarantine、legacy upgrade與error-privacy tests。合併後的exact Windows source另在real ARM64 Windows通過351項Rust tests、12-case provider-v3 CrossCheck、ARM64 PE checks與synthetic WinUI startup；macOS tests與GitHub ARM64 cross-package都不取代這些runtime assertions |
| Relink safety | If Rust changed without Swift source changes, the stale executable is removed before linking |
| Rust format | Run the repository formatter check on task-owned files; when preserving unrelated dirty source, check exact file text through rustfmt stdin so out-of-line modules are not reformatted |
| Local Rust tests | `cargo test` passes across workspace crates and test targets |
| Local Clippy | `cargo clippy --workspace --all-targets` passes, including test-only targets |


### Durable usage verification

The actual release static archive was linked into a hermetic Swift C-ABI harness, without provider requests or Keychain reads. Graph and hourly token totals and model and agent message totals agreed through truncation, partial and complete source deletion, a fresh process, restored copied transcripts, and eviction of 40 query scopes. Restored stable identities did not double-count. A legacy SQLite store whose original transcript was already gone migrated successfully; deleting its legacy cache did not change reports. The smoke exposed and fixed missing historical `sq_decisions.cache_format` adoption.

OpenCode WAL-only delete and update scenarios preserved prior rows and reconciled changed observations while the main database bytes and mtime stayed unchanged. Provider-reported costs remained authoritative; complete DB-directory removal preserved 176 tokens, three messages, and cost 0.375. A 20,000-message corpus retained 760,000 tokens across graph, hourly, models, and agents after the source tree was removed. These are correctness checks, not full steady-state resource or Windows acceptance.

The integrated owned-engine workspace, including loading-pipeline changes, passed 2,499 top-level workspace tests (six ignored), `cargo clippy --workspace --all-targets --locked`, and the release FFI build. The FFI retains warning-level Clippy debt (15 library and 22 test-target warnings); this is not a strict FFI lint gate. Both `make selftest` and `make selftest-bundled` completed with `selftest passed`, using the throwaway bundled identifier. The knowledge checker passed its 19 self-tests and repository validation.

The final release Swift bridge also exercised source-specific boundaries: Droid cumulative rewrites, empty input, restoration, and source removal retained 1,200 tokens and two messages without summing obsolete allocations; Roo UI compaction followed by sibling-only annotation changes and source removal retained 280 tokens and two messages; Kiro execution failure and removal retained eight tokens and one authoritative execution without reviving a second lower-authority snapshot. Fresh processes observed the same totals. The permanent Roo regression additionally distinguishes preserved archived annotations from refreshed live annotations.

A fresh release Swift → C ABI → Rust smoke of the integrated code preserved 116 tokens and two messages after truncation, then 244 tokens and three messages after a new turn, complete source-directory deletion, two fresh source-free processes, and restored duplicate transcripts. Each capture also accepted a primary quota scope without `clientId`. This smoke used isolated synthetic roots, not live account data or GUI acceptance.

### Live-rate freshness isolation

Automatic scheduler regression 以同一個 paused graph query 驗證 recent rate 的 `120 → 0 → 120`：先按 wall clock 過期，再同步新增事件，兩次發布都必須發生在 historical query 釋放之前。初次隔離比較中，baseline scheduler 有三個失敗斷言，修正後的 scheduler runner 通過 88 個 checks。Bounded wall-time guards 只限制失敗等待，不以 query ordering 或 mock callback 次數作為正確性證據。

實際 `DashboardModel → LiveUsageDataSource → Swift C ABI → Rust` smoke 在初次隔離及整合 Report-index／covered-lens reuse 後都使用隔離 synthetic roots 與 automatic scheduling。初始 rate 為 38；graph query 保持阻塞時，clock expiry 得到 0、新事件得到 118、手動 refresh 得到 158，source pending 在 ingestion 完成後解除。釋放舊 graph 後，manual refresh 完成且沒有 local error。原有 request-after-source barrier、owner／root／lease admission 與 first-paint pinning 不變；native fixture 另驗證 shared capture 的 228、replacement 的 268 與 clock expiry 的 0。

另一個 paused recent-query fixture 在 await 期間跨越 clock bucket 後才回傳 failure，要求 last-good rate 與有效 lease 不被該過期錯誤清除，且目前 bucket 仍可重新查詢。缺少 error-side bucket admission 時兩個斷言失敗；query 的成功與失敗都保留 clock identity 檢查。Covered-lens scope reuse 保留。RAM-first policy 改為不保留 dormant results：切離的報表再開時從 installed capture refold，但 recent／historical worker 隔離、expiry 與 late-publication contract 不變。

Header regressions also exercise eviction before a paused replacement capture returns, no repeated old-scope queries, genuine installed-lease failure and retry, reopening a popover model, and failed replacement ingestion followed by recovery. Recovery must clear all affected model errors even if a historical query's source generation is unchanged. The replacement-owned error and unchanged-generation recovery assertions failed before their respective scheduler fixes.

A separate isolated synthetic `DashboardModel → LocalUsageScheduler → LiveUsageDataSource → C ABI → Rust` executable paused the source wrapper after native replacement completed but before the scheduler received its lease. The initial header was 38 tok/min; an actual stale native query retained 38 without a false model error or repeated query, and replacement publication produced 138. An injected source-read failure then retained 138 and its genuine error; successful unchanged-generation native synchronization cleared the error. This bounded smoke used explicit scheduler turns, not visible UI or current-corpus acceptance.

這是 actual model／scheduler／native bridge 的短程 correctness 證據，不是 visible UI、current-corpus latency 或 steady-state resource acceptance。


### Phase 1 證據與未通過的 gate

先前 Phase 1 pending source 的 macOS Rust 1.96.1 formatting、locked／offline release build、1,851 tests（1 ignored）與 strict all-targets／all-features Clippy 已通過；app FFI 當時在隔離 dependency manifest 指向該 pending source 的 formatting checks、579 tests（5 ignored）、canonical non-strict Clippy 與 release library build 亦通過。這是歷史外部-manifest 證據，不當作下面 ordinary local fork checkout 的結果。Strict FFI Clippy 與 immutable baseline 有相同 warning debt，未加 lint allows，不能把它寫成 strict gate 通過。

Real C ABI synthetic OpenCode v1／v2／hybrid 的 cold／warm graph、model、hourly、Agents、rate、trace、exclusions 與 compact quota attribution 已驗證；WAL-only update／delete／insert 在 main DB bytes、mtime、size 不變時仍更新 reports，並覆蓋 partial failure／last-good、retention 與 pointer ownership。實際 Swift → C ABI → Rust scheduler smoke 驗證各 report、exact scope、cost、rate／trace clock、source-free quota、watch manifest、warm lease identity、forced synchronization、release 與 `tb_free` decoding。Fresh pending-source static library 的隔離 debug `.app --selftest` 已通過完整 suite；較早的 debug／release bare selftest 是 Phase 1 歷史證據，不代替目前 bundled release gate。

Native demo popover 已實際開啟並檢視；own-process accessibility 在無 graph demand 的 rate／quota title 得到 `25.5K/m`／`16%`，static cat／sand 的 hidden title 均為空。Menu extra 位於螢幕外，裁切圖沒有可見 icon，因此不宣稱 tray icon pixel appearance 已驗證。這些 deterministic 初始狀態不取代 shipping-identifier preference persistence 或 bundled selftest。

上述 Phase 1 是先前 pending-source integration 的歷史證據，不代替 ordinary checkout gate。現在 local fork pin 的實測另記如下；Windows Rust 1.96.1 runtime 尚未執行，macOS 證據不能代替。實際 current-corpus 冷／暖／升級輸出比較、四對 old／new 資源量測、release app 60 分鐘 steady-state 與 memory／CPU／I/O 目標仍未驗收。Safe fixture 的短程 app smoke 不得視為這些資源目標的證據。`last_verified` 記錄本次核對日期，不代表未執行的 product gates 已通過。

### Local fork ordinary checkout verification

Local fork 的 exact pin 與 publication 限制見 [`vendor/README.md`](../../vendor/README.md)。Engine code 已在獨立 branch 提交，ledger／README 是其後的 docs-only commit；consumer 只推進 gitlink。MacOS Rust 1.96.1 engine locked／offline tests 有 1,879 passes、1 ignored，release build、60 task-owned Rust files 的 scoped formatting 與 strict all-targets／all-features Clippy 通過。Real parser／cache regressions 覆蓋 bare↔split 的 materialized-first／streaming-first、changed／warm／compaction、format-3/4 retained history、non-retaining old SQLite cold miss、concurrent live authority 及 Cursor JSON read-failure propagation。

Ordinary root checkout 的 `cargo +1.96.1 test --workspace --locked --offline` 有 2,458 passes、6 ignored；canonical `cargo +1.96.1 clippy --workspace --locked --offline --all-targets --all-features` 通過但仍有既有 FFI warning debt（15 library／22 test-target warnings），不宣稱 strict FFI gate。`RUSTUP_TOOLCHAIN=1.96.1 make selftest` 與 `make selftest-bundled` 都輸出 `selftest passed`；後者使用 default throwaway `com.nyanako.tokenbar.selftest`，不是 shipping-identifier preference persistence 驗收。

以實際 ordinary-build static archive、TokenBarCore module 與 Swift decoder 執行 hermetic Swift → C ABI → Rust smoke，驗證 graph／models／hourly／Agents、cost `0.05425`、recent clock expiry、producing client-scope rejection、warm lease reuse、forced source change tokens `38→78`、replaced-generation rejection、compact quota half-open boundary／catalog、quota release 不影響 report query，以及 report release 後明確失敗。Fixture roots／pricing cache 隔離，沒有 provider quota authentication 或 Keychain 授權。

Actual `AppDelegate` demo 建立 popover；因本機 menu-extra anchor 在螢幕外，只移動 own-process panel 到可見區，再檢視其 bitmap，見 `25.5K tok/min` header 與 demo chart。這是實際 native view 的短程證據，不驗證 tray icon pixels、compositor glass/background appearance 或 shipping preferences。Windows runtime／downstream coordinated decoder、fork publication、current-corpus 對拍與完整資源 acceptance 仍未執行；沒有 push／PR／release。

`make check-docs` 通過 53 checker tests 與實際 knowledge validation。Reachability 只遞迴 Markdown files，directory／binary-asset links 仍檢查 existence，但不當作 Markdown 讀取；regression 同時覆蓋有效與 missing targets。這讓 pinned local engine ledger／routing 的 relative links 不再因 fixture directory 而 crash。

Source-watch local regressions cover rejected directory candidates and
symlinked SQLite database/WAL sampling. Per-source publication regressions
preserve captured values after late source/registry/WAL changes, ingest those
changes on the next synchronization, and reject missing or corrupt pinned
generations. The real C ABI regression verifies a late write between ingestion
and lease acceptance, dirty-baseline refresh, settled warm reuse, and last-good
queries after unreadable/corrupt-source failures.

macOS Rust 1.96.1 workspace tests passed (2,460 passes, six ignored); strict
engine Clippy passed, while workspace Clippy retains the existing FFI warnings.
Both `make selftest` and the isolated `make selftest-bundled` passed.
The installed release app's `--launch-timeline --only graph` run published its
real-data dashboard graph in 27.6 seconds without stopping other producers or
excluding active sources. Native screen capture and accessibility verification
were unavailable; no visual acceptance is claimed. Windows runtime and sustained
resource acceptance were not exercised.

The durable-migration loading regression now has two failing-before/passing-after fixtures: concurrent WAL updates must not replace the validated backup snapshot, and upstream format-10 Claude shards must migrate without the original transcript. Real Swift → C ABI → Rust `--launch-timeline --only graph` runs exercised both an isolated durable root and the installed release binary. Workspace tests, workspace Clippy (with existing FFI warnings), debug and isolated bundled selftests, and knowledge checks passed.

| Graph-publication smoke | Time |
|---|---|
| Isolated root, first migration | 82.0 seconds |
| Isolated root, new process after migration | 24.8 seconds |
| Installed release binary, actual durable root | 40.3 seconds |

These runs prove graph publication, not GUI appearance or sustained resource acceptance. The installed app was replaced and restarted after explicit approval; screen capture and accessibility access were unavailable, so no visual acceptance is claimed. Private diagnostic data and rollback material remain outside the repository.

### Incremental source synchronization experiment

Existing same-identity Claude／Codex primary-file changes reuse the previous exact scope's immutable source references and recollect only the changed primaries. Permanent fixtures cover Report／LocalClients parity, copied duplicates, multiple changed files, stale and mismatched scopes, sibling fallback, incomplete Codex reads, Claude same-key corrections and retained facts after truncation／deletion. Manifest fixtures cover sampled content changes with preserved mtime／size, immutable pre-ingestion baselines, late writes, missing／replaced files, WAL changes, retained-history drift and bounded-plan overflow. The actual C ABI regression also exercises a write after targeted ingestion and observes it only in the next capture.

A fresh release C ABI smoke over isolated synthetic roots preserved exact totals through append (348 → 446 tokens), same-key correction (506 tokens), new-source discovery and source deletion (624 tokens). Graph／hourly／models／Agents matched authoritative ingestion; clock-only rate expiry returned zero, and an incomplete Codex write failed without invalidating the last-good scope. No provider request or Keychain access was used.

The actual Swift `TBCore.localUsageSync`／`localUsageQuery`／`releaseLocalUsage` implementation linked against the fresh Rust static archive and exercised 76 → 154 tokens, unchanged lease reuse and clock-only rate expiry. Final workspace tests passed 2,507 top-level cases plus one child-process lock check, with six ignored; workspace Clippy passed with the existing FFI warning counts (15 library／22 test-target warnings). Both `make selftest` and the isolated `make selftest-bundled` passed. Exact task-owned Rust formatting and the knowledge checker (19 self-tests plus repository validation) passed.

The same fresh release binary compared authoritative full synchronization with targeted synchronization using equivalent restored source／store seeds. Each workload ran four pairs in both orders (full → targeted, targeted → full, repeated), with a fixed two-second pause before each arm. Each mutation appended one Claude and one Codex event. All report oracles matched; every pair improved synchronization time and remained below the preregistered +5% synchronization-plus-graph regression limit.

| Synthetic workload | Full sync median | Targeted sync median | Median paired sync improvement | Worst paired sync improvement | Full sync + graph median | Targeted sync + graph median |
|---|---|---|---|---|---|---|
| 1,200 files／2,400 events | 766.5 ms | 145.1 ms | 81.1% | 80.7% | 807.9 ms | 186.7 ms |
| 48 files／12,288 events | 154.5 ms | 119.1 ms | 23.0% | 21.4% | 291.7 ms | 259.2 ms |

The process-attributed physical-write counter medians decreased from 87.9 to 12.3 MiB and from 24.0 to 21.4 MiB respectively. Physical-read counters were zero on these warm workloads and cannot establish reduced logical reads. Peak RSS medians were effectively unchanged (29.6 → 29.7 MiB and 52.8 → 53.2 MiB); no memory improvement is claimed. These short synthetic runs do not prove GUI／FSEvents latency, real-corpus performance, cold-load improvement, Windows behavior or sustained resource acceptance. Raw measurements and temporary diagnostic material remain outside the public repository.




### Report-query and lens-reuse performance experiment

The ordinary and targeted publishers prepare an exact-generation Report v2 selected-row index before GC. Regression cases cover client eligibility before Claude／Codex arbitration, duplicates across separate source IDs, synthetic-only shared identities, complete zero-winner markers, unchanged publication reuse, old-marker fallback, deleted duplicate losers and corrupt winners. Graph／model／hourly／Agents must agree, while OpenCode predicate changes still recompute fallback authority. TEMP batching retains failure-before-callback and reentrancy; public graph APIs retain session/time metrics.

Quota catalog cases cover moving bounds, distinct accounts and snapshots, empty and oversized catalogs, expired leases and pricing growth near the registry budget. Optional catalog retention cannot truncate a response or introduce a valid-response failure. Scheduler cases cover covered lens switches, scope additions, force and dirty recollection, dormant-result eviction, recent-event expiry, root／price／owner changes, late publication and final-close lease release.

A fresh release C ABI comparison used 120 Claude files／2,880 events and 2,000 OpenCode events, frozen synthetic pricing and isolated source／store roots. Graph／model／hourly／Agents totals were exactly 127,440 tokens／4,880 messages. Repeated graph and quota calls matched the baseline; narrowed quota bounds changed diagnostics without changing the catalog. Normalization removes only volatile timing／generation fields and order-insensitive client lists; token and cost changes remain detectable. A same-key correction increased totals to 127,450; truncation, complete source removal and a new source-free process retained those totals. Superseded and released leases failed closed.

The actual Swift scheduler linked against the fresh Rust static archive, not a replacement source. For the models → graph → models → graph sequence, source-sync count stayed 1 instead of the baseline's 1 → 2, and pure-query count stayed 3 instead of 4 → 6. Forced refresh still performed a new synchronization. The isolated bundled app's graph-only launch probe reached DashboardModel's graph milestone; it did not run provider polling or save a dashboard restart snapshot. This is execution evidence, not visual UI acceptance.

The paired comparison used four old／new pairs in alternating orders, two seconds before each process, and cold plus fresh-process warm stages. Every one of the 16 runs matched the exact normalized baseline oracle. The query bundle is graph first／repeat plus models／hourly／Agents; source synchronization is measured separately. All pairs passed the preregistered limits: at least 20% cold query improvement, no more than 5% warm query regression, and no more than 25% cold peak-RSS regression. Worst cold query improvement was 37.1%; largest cold peak-RSS increase was 7.2%.

| Synthetic metric (median) | Baseline | Optimized |
|---|---|---|
| Cold report-query bundle | 413.8 ms | 255.8 ms |
| Fresh-process warm report-query bundle | 410.3 ms | 260.0 ms |
| Cold repeated quota fold | 104.6 ms | 55.3 ms |
| Warm repeated quota fold | 102.0 ms | 56.8 ms |
| Warm narrowed quota interval | 51.5 ms | 1.1 ms |
| Cold source synchronization | 504.1 ms | 553.2 ms |
| Warm source synchronization | 204.5 ms | 211.7 ms |
| Cold peak RSS | 102.9 MiB | 102.7 MiB |
| Warm peak RSS | 95.0 MiB | 83.9 MiB |

Cold synchronization pays the once-per-generation Claude／Codex index build; its median increased 9.7%. One optimized cold capture took 2,107.0 ms and remains in the raw results; its cause is not established. The initial variants missed the query threshold, and runtime sampling identified fingerprint-order wide-row reads and a second canonical read for primary identity. Scope-first loading and TEMP identity projection removed that repeated work without changing authority decisions. No raw run or outlier was discarded. These short synthetic measurements do not establish improved initial first paint, production-corpus latency, sustained RAM reduction, GUI／FSEvents behavior or Windows acceptance. Screen-capture and Accessibility permissions were unavailable; actual native model execution is not visual evidence. Diagnostic sources and raw evidence remain outside the public repository.

Final `cargo test --workspace --locked` passed 2,517 top-level cases plus one child-process lock check, with six ignored. Workspace Clippy passed with the existing FFI warnings (15 library／22 test-target); exact task-owned Rust formatting, both `make selftest` and `make selftest-bundled`, isolated bundle signature verification and the knowledge checker (19 self-tests plus repository validation) passed. Rust language-server diagnostics were unavailable, so compiler／Clippy checks provided Rust verification; Swift scheduler diagnostics retained the baseline's three SwiftLint errors and eight warnings, not a clean lint result.


### Named source hint verification

The optional `changedFileURLs` sync field is verified at the Swift／C ABI／Rust seam. Native fixtures cover same-key rewrites, unchanged authoritative reuse, malformed hints, parser sibling drift, replaced files, later unhinted changes and rejection of a replaced lease. Scheduler fixtures cover bounded coalescing, sticky unnamed／overflow fallback, retry／in-flight events, active all-known-input deadlines under continuous hints, forced verification and graph-before-hourly／Agents-before-models issue order. Existing report arbitration, corruption and completeness fixtures remain the semantic guards for metadata-only publication scratch.

A production release-library smoke used 500 historical Claude rows plus a recent event: graph 19,038 → 19,146, rate 38 → 146 → clock-expired 0, empty-year graph 0. Graph／models／hourly／Agents outputs after the changed capture matched unchanged and forced captures, excluding only the generated-at timestamp.

An actual automatic Swift scheduler linked the fresh static archive and used `LiveUsageDataSource` with a real FSEvents stream over an established synthetic file. Appending an event published graph／rate 38 → 146 in 306.1 ms, then the rate expired to 0; a post-request forced refresh read 194 graph tokens, with no current query errors. The typed native request contained named event evidence. Newly created fixture roots also emitted structural flags and correctly took the authoritative fallback instead of the named path; named hints are not promised for every OS event.

Four synthetic pairs compared the same release binary with all-known verification versus named verification, alternating arm order and pausing two seconds before each arm. Each fresh isolated arm contained 601 Claude files with 1,101 initial messages and appended one event to the existing primary. Each arm asserted 35,838 → 35,946 graph tokens, recent expiry, and changed／warm／forced report equality.

| Median synthetic metric | All-known verification | Named verification |
|---|---|---|
| Changed-source synchronization | 113.573 ms | 81.916 ms |
| Synchronization plus first graph query | 127.240 ms | 95.410 ms |

All four named arms improved both metrics; the 245.433 ms all-known synchronization outlier remains included. These fresh isolated synthetic arms do not prove production-corpus latency, first paint, sustained CPU／RAM improvements, or visible GUI acceptance. Replacement-manifest copying and historical parser／report work remain history-scaled; Claude append parsing and reversible delta aggregation were not introduced.

`cargo test --workspace --locked` passed 2,518 top-level tests plus one child-process lock check, with six ignored. Workspace Clippy passed with the existing FFI warnings (15 library／22 test-target). `make selftest` and `make selftest-bundled` passed; the bundled gate used the default throwaway identifier, not shipping preferences. Exact task-owned Rust formatting and isolated bundle signature verification passed. Rust LSP was unavailable; compiler／Clippy supplied Rust verification. No installed application, commit, remote publication or Windows runtime was involved.

### RAM-first ownership and source streaming

Memory ownership 以 disk-backed facts 與 demanded display aggregates 分開：OpenCode fresh key／alias membership、MiMo duplicate state、Zcode turn ranking 與 parser staging 都使用 file-backed scratch；Pi-family／OpenClaw／MiMoCode／Zcode／Copilot Desktop app ingestion 不建立 source-sized message vectors。Regression cases cover nonempty fresh OpenCode transfers carrying more than 1,024 old rows, alias-only overlap, carried multiplicity／order, metadata-only versus full Report arbitration, source-free retained facts, and partial producer failure without publication. Zcode WAL mutation occurs between its two source passes and must preserve the original snapshot's rows、timestamps and turn winner. OpenClaw exact 64 MiB decoded input succeeds, limit-plus-one／truncation fails, and known message timestamps survive decoding.

Swift cases require lens-away results to be released, reopened lenses to refold without unnecessary source recollection, and irrelevant clock notifications to reuse demanded historical aggregates without DB refolding. Recent expiry、source／price identity、owner replacement and forced-refresh barriers remain independent. Frame selection preserves style／level／appearance and parrot geometry, with a static icon loading one frame. FFI buffer ownership is exercised through real native responses, not decoder mock echoes.

An isolated release C ABI workload ingested 12,000 OpenCode rows、12,000 OMP messages and 2,000 Claude messages. Cold／warm reports agreed at 388,000 tokens／26,000 messages. Updating every fifth OpenCode row and deleting the next fifth yielded 436,000 tokens with the same message count; complete source removal retained those results. The earlier implementation failed the more-than-1,024-row carry with an absent `oc_batch` savepoint; the bounded carry completed and released leases reject later queries. Separate 1,500-row source DB fixtures preserved MiMo 31,500 tokens、Copilot 240,000 tokens and Zcode 180,000 tokens／750 turns after source removal.

A fresh Swift → static C ABI → Rust harness decoded graph／models／hourly／Agents at 38 → 138 tokens, retained 138 after source removal, and observed recent expiry to zero. Sixty-four real success／error response cycles and released-lease／NULL rejection exercised the custom `tb_free` deallocator. These are hermetic native data-flow checks, not live credentials or GUI acceptance.

Four old／new pairs ran in alternating orders, with three seconds before each process. Both arms included the concurrently integrated named-source-hint／metadata-only-publication changes, isolating this RAM-first delta. Each fresh arm ran the same 26,000-message workload, updating／deleting every fiftieth OpenCode row so the baseline's carry-batch defect was not triggered. All eight attempts completed and all four report phases matched the exact normalized oracle, including token lanes、cost and messages. Normalization removed generated timestamps and canonicalized only unordered day-client／trace entries and tied model rows; mutations of provider、model、cost、message count and input tokens changed the oracle, while unordered-client reversal did not.

| Synthetic whole-process metric | Baseline median | RAM-first median | Paired median change | Worst pair change |
|---|---|---|---|---|
| Peak RSS | 229.72 MiB | 61.16 MiB | −168.56 MiB（−73.3%） | −145.63 MiB（−69.8%） |
| Peak physical footprint | 76.17 MiB | 32.12 MiB | −43.19 MiB（−56.7%） | −38.64 MiB（−55.2%） |
| Full workload time | 16.43 s | 22.43 s | +6.76 s（+41.3%） | +53.9% |

Per-arm medians are descriptive; paired deltas and ratios are calculated before taking their medians. Time covers native synchronization／queries, not a user-visible refresh deadline. The memory-priority policy accepts the additional disk／computation latency. `/usr/bin/time -l` sampled the same whole processes, including the Python fixture driver. This is short synthetic evidence, not installed-app、current-corpus、first-paint、60-minute resource or Windows acceptance; raw evidence remains outside the public tree.

The actual native sand tray renderer kept 144 frames at 24 fps in both Debug arms. One observation per arm showed footprint 45.7 → 31.6 MiB and `NSImage` count 1,328 → 290; PNG reader objects fell from 1,182 to 144. This isolated renderer path does not poll sources or quota providers. Screen capture permission was unavailable and the status item has no own-process window, so icon pixels／Settings interaction and the compositor are not visually verified. It is not a four-pair shipping-GUI or long-duration RAM acceptance result.

Final workspace Rust tests passed 2,532 top-level cases plus one child-process check, with six ignored. Workspace Clippy passed with the existing 15 library／22 test-target FFI warnings. Fresh release Rust linkage、debug and default isolated-identifier bundled selftests、bundle signature verification and all 15 task-owned Rust formatting checks passed. Repository-wide `cargo fmt` still reports the same five unrelated files as the immutable baseline; those files are byte-identical and were not reformatted. Rust LSP was unavailable. Seven changed Swift files retained the baseline's 281 SwiftLint errors, with no added error rule/count; this is not a clean lint result. The knowledge checker passed 19 self-tests and repository validation.

## Local build and UX acceptance

> **⚠️ `make run` 與 bundle 讀的不是同一個 `UserDefaults` 網域。** `swift run Syrtis`（`make run` 就是它）產出裸執行檔、沒有 `CFBundleIdentifier`，`UserDefaults.standard` 因此落在行程名網域 **`Syrtis`**；bundle 用 **`com.nyanako.tokenbar`**（背景見本文件上方 bundle identity 段落）。兩份偏好互不可見，且裸執行檔那一份會隨開發過程被寫入，內容與使用者實際設定無關。
>
> 凡是驗收由偏好驅動的畫面——**usage attribution 宣告、Settings 持久化、狀態列項目狀態**——一律用 `make bundle` 產生的 `dist/Syrtis.app`，並在**出貨 identifier 下**跑，前後備份還原使用者的偏好：
>
> ```bash
> # 驗收前：先退出 /Applications/Syrtis.app，兩者同網域不可並行
> BACKUP=$(mktemp ~/tokenbar-prefs-XXXXXX)
> defaults export com.nyanako.tokenbar "$BACKUP" \
>   && plutil -lint "$BACKUP" \
>   && echo "備份完成：$BACKUP"   # 沒印出來就不要往下做
>
> # ……驗收……
>
> # 驗收後：先結束受測 app 並等它真的退出
> osascript -e 'quit app "Syrtis"' 2>/dev/null || true
> while pgrep -f 'dist/Syrtis.app' >/dev/null; do sleep 1; done
> # 備份不可用就停手：寧可留著髒偏好，也不要刪掉沒有備份的網域
> plutil -lint "$BACKUP" \
>   && defaults delete com.nyanako.tokenbar \
>   && defaults import com.nyanako.tokenbar "$BACKUP"
> ```
>
> **`mktemp` 與兩次 `plutil -lint` 都不是裝飾。** `defaults delete` 是破壞性的；若備份失敗而刪除仍照跑，結果是使用者的正式偏好被清掉且無從還原，或被上一次跑剩的舊檔蓋回去。三個環節各擋一種失敗，沒有一個能取代另一個：`mktemp` 保證檔名不重複（時間戳做不到，兩次執行落在同一秒就會撞）；`&&` 串住 export 的結束狀態；`plutil -lint` 擋掉「寫到一半失敗但檔案非空」——實測磁碟寫入截斷的 plist 會讓 `test -s` 通過而 `plutil -lint` 回非零。還原前再 lint 一次，因為備份是在驗收之前做的，中間隔著人的操作。
>
> 路徑刻意放家目錄而不是 `mktemp -t` 的 `/var/folders`：那裡重開機會整棵清掉，而這份備份是使用者偏好的唯一副本。`mktemp` 的模板 `XXXXXX` 必須在字串結尾，接副檔名不會被替換（實測會生出字面叫 `XXXXXX` 的檔）；`defaults export`／`import` 不需要 `.plist` 副檔名。跨終端機做驗收時把印出來的那個路徑帶著。
>
> **結束 app 那兩行不可省。** 還原一個「還有行程在寫」的網域，本質上就是無效的：受測 app 在前景時持續輪詢額度並寫回自己的偏好（例如 `TrayAnimator.swift:221` 的 `tokenbar.quota.lastRemaining`），所以它可以在 `defaults import` 跑完之後再蓋一次。選單列 app 沒有視窗，最容易忘記它還在。
>
> **`delete` 那一步也不可省。** 實測（2026-09-13，於無關網域）：`defaults import` 是**合併**不是取代——驗收期間新增的鍵會存活下來，只有備份時就存在的鍵會被還原成舊值。先 `delete` 整個網域再 `import` 才會完全還原。
>
> **不要改用一次性的 `BUNDLE_ID` 來迴避備份**，即使那看起來更乾淨。本文件上方已經對同一個旋鈕做過完整推理並得出相反結論：拋棄式 identifier 是**比較弱的 gate**，抓不到以出貨字串本身為條件的值。當場就有實例——`SnapshotStore.swift:364` 要求 bundle id **字面等於** `com.nyanako.tokenbar`，換了 identifier 就整個關掉 restart snapshot，那個面因此無法驗收。
>
> **identifier 只隔離偏好，隔離不了資料。** `crates/tb_core_ffi/src/agent_quota_history.rs:454`、`:518` 與 `agent_account_scope.rs:220` 都是 `dirs::data_dir()` 接上**寫死的** `com.nyanako.tokenbar`，不由 bundle id 推導。所以本機 UX 驗收無論用哪個 identifier，都會讀寫使用者正式的 quota-pace history 與 account-scope storage。`~/Library/Caches/SyrtisDashboardSnapshot` 同樣是固定路徑。**而且這不限於刻意驗額度的場合。** 非 demo 的 bundle 一啟動，`TrayAnimator.start()`（`TrayAnimator.swift:180`）就無條件進入額度輪詢，走生產路徑的 `enrich_snapshot`（`agent_usage.rs:1504`）把觀測寫進那份固定路徑的 store。所以**任何**非 demo 的本機 bundle 驗收都在寫使用者的正式歷史庫，連只看 Settings 或狀態列的也是。要嘛連那份 store 一起備份還原，要嘛改用 `--demo`。

> 這條規則來自一次實際的假回歸。Codex 時間窗歷史卡的每一列都顯示零 token 與零金額，並印出「額度變動了 N%，但這台機器上沒有記錄到」，而同一台機器上安裝的 bundle 顯示正常；當時 engine pin 剛推進過，於是看起來像那次推進造成的回歸。實際鏈條與 engine 無關：`tokenbar.usage.attribution.confirmed` 在兩個網域的內容不同，受測 client 在 bundle 網域有宣告、在行程名網域沒有，於是 `UsageAttribution.resolve` 回 `.unassigned`，`QuotaHistory.swift` 的 `spanTotals` 歸屬閘門把該 span 的每一則訊息都跳過，`spanTokens` 與 `spanCost` 皆為 0，`WindowEquivalence.aggregate` 因此回 `.unaccounted`。週期本身讀固定路徑的 `quota-pace-history-v3.json`，不受網域影響，所以列仍在——這正是它看起來像資料缺失而非組態差異的原因。

> 同時排除掉的方向，記下來避免重查：推進前後兩個 engine pin 對同一批 span 回傳位元相同的訊息數與 token 總量，隔離快取也不改變結果——該鏈條不經過 `UsageAttribution`。

> **不要**讓裸執行檔改讀 `UserDefaults(suiteName: "com.nyanako.tokenbar")` 來迴避這件事：那會讓開發執行檔寫進使用者正式的偏好網域，摧毀 `SELFTEST_BUNDLE_ID` 建立的隔離。

不需要 `.app` bundle 語意、且不依賴 `UserDefaults` 的人工 UI 檢查，優先從 repository root 執行 `swift run Syrtis --open-popover`。需要 `make bundle` 產生的 `dist/Syrtis.app` 的有兩類：一是 icon、`Info.plist`、`LSUIElement`、Sparkle、autostart 或安裝路徑等 bundle-only 行為；二是**任何由偏好驅動的畫面**（usage attribution、Settings 持久化、狀態列項目狀態），且必須照上方網域警告的備份還原程序走。以 Argument Domain 注入初始偏好的 deterministic 檢查**不需要備份還原**，因為它們跑在裸執行檔上、只碰 `Syrtis` 網域，動不到使用者正式的偏好。但**不要以為它們因此就與既有網域無關**：Argument Domain 只覆寫命令列傳進去的鍵，其餘一律往下落到 application domain。例如 `SettingsWindowView.swift:56` 讀 `tokenbar.tabs.hidden`，而下方那組指令沒有注入它，於是開發過程累積在 `Syrtis` 網域的舊值會改變你看到的列。這類檢查的 deterministic 只在「該畫面的每一個輸入都被注入」時成立；做不到就先 `defaults delete Syrtis` 清掉再跑。

Provider quota pace 以 `swift run Syrtis --demo --open-popover` 提供 deterministic 人工驗收面；snapshot badge 明示 `FIXTURE`，且 `DemoUsageDataSource` 不呼叫 live FFI、不讀寫 quota cache。Historical／Linear／Off 都要實際呈現；驗收時必須區分低 remaining 觸發的 quota 長條黃／紅健康色，與 deficit stage 觸發的 pace marker／footer 橘色。橘色只看 actual 有沒有越過 expected 線，不看是哪個 estimator 畫出那條線——Historical 與 Linear 的 deficit 同色，狀態文案仍必須分辨兩者。舊規則（只有 `available` 可上色）已廢止：`available` 由每次 refresh 重跑的 out-of-sample fit gate 決定，同一張卡會在 Historical 與 `learningHistory` 之間來回，把顏色綁在 basis 上會讓使用者看到預測「一下子就不見了」，而底層 deficit 其實一直存在。

Menu bar 的「Font color」驗收涵蓋 Automatic／Custom 切換、正常／偏低／不足三個獨立色塊下方的 16 色面板與 HEX 輸入、深淺預覽、主項目與獨立用戶端數值未改變時立即換色，以及切回 Automatic 後恢復額度／系統字色。以獨立 UserDefaults suite 驗證舊單色設定保留為正常色、另外兩色的預設、三色保存後重讀、25%／10% 邊界與邊界上方的色階選擇、非額度文字／無額度使用正常色、無效儲存值只回退該檔位，以及恢復自動時保留自訂值；同時鎖定自動 gauge 的原始色值。這些 selftest 不取代實際互動。GUI 需確認面板位於自訂顏色列下方、沒有系統調色盤，連續切換三個色塊會更新同一面板的編輯檔位，分別換色不改動其他兩色，並驗證輸入 `#000000`、小寫與省略 `#` 的正規化，以及未完成／無效輸入不改動上一個有效顏色，並在輸入框失去焦點時清空。Argument Domain 注入可驗初始配色，不代表互動寫入或跨 process 持久化已驗證。

Individual client items的deterministic Settings檢查以Argument Domain注入初始偏好；這只驗visual state與initial routing，不在同一process宣稱toggle persistence：

```bash
swift run Syrtis --demo --settings \
  -tokenbar.tray.clients.enabled claude,codex \
  -tokenbar.tray.clients.quotaSelections '{"claude":"auto","codex":"weekly.v1"}'

swift run Syrtis --demo --settings \
  -tokenbar.tray.clients.enabled claude,codex \
  -tokenbar.tray.clients.quotaSelections '{"claude":"auto","codex":"missing.v1"}'

swift run Syrtis --demo --settings \
  -tokenbar.tray.clients.enabled claude,codex \
  -tokenbar.tray.clients.quotaSelections '{"claude":"auto","codex":"weekly.v1"}' \
  -tokenbar.tabs.hidden codex
```

> **本機 bundle 邊界：** `dist/Syrtis.app` 是暫時的驗收產物，不是第二份安裝。日常使用與正式更新的 source of truth 仍是 `/Applications/Syrtis.app`。
>
> `make selftest-bundled` 另外會在 `dist/selftest/Syrtis.app` 產生同名 bundle。它刻意與出貨同名同 identifier（見上方 gate 段落），所以**兩者不可混淆**：UX 驗收與下方清理程序談的一律是 `dist/Syrtis.app`。selftest 產物只被直接執行、在 app lifecycle 之前就結束，不會被 LaunchServices 註冊；`bundle.sh` 也會在 `dist/selftest/` 放一份 `.metadata_never_index`。不需要時整個目錄刪掉即可。

[`scripts/bundle.sh`](../../scripts/bundle.sh) 會在組裝 app 前建立 `dist/.metadata_never_index`，避免 Spotlight 主動索引本機 bundle。但這個 marker 不會回溯刪除既有 Spotlight metadata；實際啟動 `dist/Syrtis.app` 也可能讓 LaunchServices 註冊它。因此本機 UX 驗收完成、且不再需要該 bundle 作為 release artifact 時，應撤銷這個特定 app 的註冊並刪除生成物，不要以重設整個 Launchpad database 作為第一步。

| UX surface | Preferred path | Completion evidence |
|---|---|---|
| Popover、lens、keyboard、scroll、appearance | `swift run Syrtis --open-popover`；但 lens 記憶與 appearance 的互動寫入是偏好驅動的，那部分依上方網域警告改用 bundle 加備份還原 | 實際操作與必要截圖；結束測試 process |
| Individual client status items | `--demo --settings`配合兩個M2 Argument Domain keys驗initial visual state；本機資料、placement／right-click／跨螢幕則用同一`dist/Syrtis.app`（placement 記憶存在 app 自己的偏好網域，依上方網域警告備份還原，不要換 identifier——換了等於記憶歸零） | 預設只有主item；switch點擊後立即以`.mini`原生狀態更新，client shell可稍後於同一defaults reconciliation建立但不得阻塞control setter；Settings body重建不得同步呼叫`SMAppService.status`（本機量測單次約0.5～0.9秒），關閉／重開與連續toggle都須維持可互動；enable／disable／hide／restore保留selection與`tokenbar-client-<id>` placement；`antigravity-cli`在本機error-only provider狀態仍可配置；dashboard選定單一年份時，Settings仍以all-time graph保留所有live client item rows並可停用；client A→B沿用同一popover並各自恢復本次app session停留的lens；主item恢復自己的client-plus-lens route，不被individual item覆寫；主item right-click仍只改global source；1x／2x雙向移動圖示清晰；VoiceOver label不含raw card／error，explicit error fallback固定讀作last-known而非current quota；0與8 items的idle profile沒有per-client loop |
| Icon、bundle identity、Sparkle、autostart | `make bundle` 後啟動 `dist/Syrtis.app` | 記錄 bundle-only 行為；完成後 unregister 並移除本機 bundle |
| Homebrew、Sparkle stable update、正式安裝路徑 | `/Applications/Syrtis.app` | 不以 `dist/Syrtis.app` 代替 installed-app 驗收 |

從 repository root 清理已完成驗收的本機 bundle：

```bash
ROOT="$(git rev-parse --show-toplevel)"
LOCAL_APP="$ROOT/dist/Syrtis.app"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

test -e "$ROOT/dist/.metadata_never_index"
"$LSREGISTER" -u "$LOCAL_APP" 2>/dev/null || true
rm -rf -- "$LOCAL_APP"

```

清理後，Spotlight 與 LaunchServices 查詢都不應再列出 repository 的 `dist/Syrtis.app`；正常情況只保留 `/Applications/Syrtis.app`：

```bash
mdfind "kMDItemContentType == 'com.apple.application-bundle' && kMDItemFSName == 'Syrtis.app'"
"$LSREGISTER" -dump | grep -F 'Syrtis.app'
```

若仍有 stale 結果，先等待 metadata service 收斂並重查；不要直接清空整台機器的 Spotlight index 或重設 Launchpad，因為那會波及其他 app 與使用者排列。

## Cache and sibling checks

A source reader that consumes secondary files must be verified as one unit. The regression matrix is deliberately broader than the parser function itself.

| Seam | Check |
|---|---|
| Fingerprint | Primary-only change and sibling-only change produce different fingerprints |
| Active lane | The source is reachable by the streaming and materialized consumers that claim support |
| Latest mtime | Live-tail change token observes every relevant sibling and WAL |
| Pruning | `modified_after` keeps a session when a relevant sibling is fresher than the primary |
| Cache rebuild | Same-fingerprint stale serialized data is rejected when parser output or attribution changes |
| Report parity | Materialized and streaming reports agree on the fixture's selected fields |

## Cross-language invariants

| Contract | Verification |
|---|---|
| Heap JSON ownership | Every successful FFI pointer is decoded and released through `tb_free`; errors do not leak a second ownership path |
| Envelope shape | `ok` and `data`/`err` fields match `ctb.h` and Swift decoders; agent usage classifies `ok:false` as fixed bridge failure and malformed or `ok:true` missing data as fixed decode failure without public associated text |
| Publication order | Rust `publicationGeneration` is additive, gate-owned, and checked against exhaustion; one shared Swift `@MainActor` coordinator rejects lower generated payloads across DashboardModel, Settings, tray polling, and snapshot restore before payload or scalar apply, while missing generations pass through without changing state |
| Windows compatibility | M19-B1's production C# decoder ignored additive `publicationGeneration` while preserving existing fields; no DTO, C signature, or ownership change was required |
| Windows storage ownership | Owned by Syrtis-Windows since #276 ([Syrtis-Windows#147](https://github.com/Nanako0129/Syrtis-Windows/issues/147)); its copy of `crates/tb_core_ffi` is the source of truth for the CNG／DACL／secure-open／identity／lock／replace／quarantine primitives and their tests. This repository carries none of that code |
| Client filter | Non-empty selected IDs reach Rust before mixed buckets are folded; `nil`／empty client lists mean all clients per `ctb.h`; the Swift lens strict-membership check blocks all-hidden views |
| Arithmetic | Rust report totals, FFI mappers, Swift models, and live-rate consumers use bounded arithmetic where required |
| Stale-data policy | A failed refresh retains the last good value instead of blanking a working card；fallback preserves display-ready fields only for a structural same-binding request transient, marks account scope untrusted, and skips enrichment／history／re-cache. Claude and Grok validate the response, preflight the exact durable target before lineage transfer, then typed-save through a second exact re-read／compare；`TargetMissing`／`TargetChanged`／`TargetMalformed`／`TargetUnverified` are terminal with zero required usage continuation, while only a persistence-stage failure after the second compare succeeds may return the fresh credential and transferred scope with no cache binding. `CredentialsPersisted` is the after-attempt checkpoint even when typed save returns an error. Codex and Antigravity retain their existing ordering and failure policy；Codex save failure leaves auth and metadata bytes unchanged, credentials persist before lineage transfer, transfer failure conditionally rolls back only while the exact just-written root still matches, and its post-binding reuses the verified scopes. External writers can still race compare-to-write／rollback, Keychain `security -U` is not CAS, and credentials plus metadata are not a cross-resource atomic transaction |
| Diagnostic wire | Rust emits only `{category,status?,osCode?}` from the fixed category allowlist；`rateLimited`只允許status 429且不帶osCode，`serverError`只允許500...599且不帶osCode，非HTTP categories不得帶status，unknown category不得帶associated numerics；Swift lossy decoding逐欄丟棄malformed optional integers，非object或缺失／非字串category不建立candidate、unknown values正規化，且public logging維持bounded |
| Swift scalar authority | Successful finite outer payloads replace memory and persistent scalar; Dashboard polling reconciles accepted publications immediately and TrayAnimator reads the coordinator's latest generated payload before its own poll completes; selection／exclusion changes re-reconcile before render; Settings task identity uses `publicationGeneration` or a legacy timestamp-plus-resolved-scalar fingerprint; terminal／Absent／unresolved／hidden-all clear the scalar; only outer FFI failure or missing payload retains it. Isolated UserDefaults tests cover restart non-revival, healthy replacement, explicit fallback windows and their snapshot-time 30-minute stale boundary (#8), Auto exclusion, reconciliation identity collisions, a newer terminal result followed by a late older tray success, and a newer Dashboard success replacing an older tray payload plus scalar while invalidating the gauge-render signature |
| Quota curve identity | Series identity 不跨 ABI：Rust 擁有 publication-owned binding table，Swift 只傳 `clientId`／`windowKey`／`publicationGeneration`。Binding 只在 serialization 成功後替換，generation 不符為錯誤而非 fallback，binding 在 history I/O 後重新解析以拒絕已移動的 identity；absent history 的 `ok:true` + `null` 由專屬 optional-envelope decoder 處理，不得與 decode failure 混同 |
| Historical pace | Rust 的 typed `paceStatus` 擁有 lifecycle／duration，optional nested result 同時擁有 expected、ETA、will-last 與 risk；Swift 只能導出 mode policy、stage 與文字。`available + completeCycles == 0` 是合法的 validated current-window projection：Historical mode 必須使用 backend expected／ETA／will-last，partial risk 缺失時維持 nil、不顯示 risk copy，也不得退回 Linear。只有 `learningHistory` 可明示使用 exact-duration Linear estimate，`learningDuration`／`unavailable`／legacy 不得 silent fallback |
| Lifecycle | Closing a popover or settings window cancels its tasks and stops background rendering |
| Restart snapshot | Disk bytes carry only the aggregated graph payload plus schema/build identity, proven by a recursive exact-key assertion with a canary in every excluded field, not a keyword scan; any shape change bumps `snapshotSchemaVersion`. Restore is rejected for schema、bundle、version or exact-build mismatch, a requested year the snapshot does not match, malformed `knownYears`, a `savedAt` beyond the clock-skew or retention bounds, corrupt bytes, the byte cap and cap+1, a symlinked file or directory, a FIFO, a directory in the file's place, and wrong owner or mode. `--selftest`、`--smoke`、`--demo` and `--icon-gallery` must yield no shipping identity, asserted against an injected production bundle triple with a control proving that triple alone does yield one；a spy fails if the production directory is even resolved. Ordering is covered in both task orders, a superseded fetch must not clear a newer request, and mutation targets include the build comparison、the requested-year gate、the regular-file check、`O_NOFOLLOW`、the content digest、the capture sequence and the indicator's ownership check |

### Credential refresh target matrix

Claude File、Claude pure Keychain decision and Grok exact-entry owners exercise the shipping continuation seams rather than returning a prebuilt failure or credential tuple. Each row first uses `TestRefreshScope` to establish A's lineage and only then captures the metadata baseline. Claude routes a real hermetic `refresh_claude_credentials_with` result through `fetch_claude_login_usage_with`; the synthetic Keychain row supplies account／item reads to the pure decision helper and never invokes `/usr/bin/security`. Grok routes an adapter that actually calls `refresh_credentials_with` through `retry_grok_weekly_after_auth_with`; its counter covers only the required weekly retry, never the additive monthly request.

| Phase／class | Required observation |
|---|---|
| Network-wait `TargetChanged`／`TargetMissing`／`TargetMalformed`／`TargetUnverified` | Terminal；request and preflight exactly once；no transfer、typed save、writer or usage／weekly retry；metadata byte-identical to the established-A baseline；external durable state preserved. Claude also owns explicit logout and mcpOAuth-only as `TargetMissing` without fallback |
| Post-preflight four target classes | Terminal；request、preflight、transfer and typed save exactly once；writer and usage／weekly retry zero；checkpoint sequence includes `MetadataHandled` and the after-attempt `CredentialsPersisted`；external durable state preserved；the transferred new marker may remain as inert A-lineage metadata |
| Genuine persistence failure | Second exact compare succeeds before the injected writer failure；new access credential and transferred A scope reach exactly one required continuation；cache binding is `None`；old durable bytes remain；old／new markers share lineage |
| Success with concurrent sibling write | Exactly one required continuation；reusable binding resolves to A；Claude top-level sibling or Grok foreign／root／entry siblings survive the merge；Keychain output remains pinned to captured account A |

Common assertions use only fixed secret-free labels: `REFRESH-TARGET-PREFLIGHT-TERMINAL`、`REFRESH-TARGET-POST-PREFLIGHT-TERMINAL`、`REFRESH-TARGET-USAGE-BLOCKED`、`REFRESH-TARGET-PERSISTENCE-FAILSOFT`、`REFRESH-TARGET-KEYCHAIN-PIN`、`REFRESH-TARGET-SIBLING-PRESERVATION` and `REFRESH-TARGET-CHECKPOINT-SEQUENCE`. Failure output must not include a token、account、path、JSON body or raw metadata. A preflight owner must not probe `resolve_current(new_marker)` to prove absence because that helper can create the mapping；metadata bytes plus transfer count own that assertion.

Mutation qualification is thirteen compile-preserving changes: four independent typed-save target-class-to-`Persistence` mutations, plus preflight ordering、second re-read／compare、Grok exact-entry selection、Keychain account pinning、persistence cacheability、persistence terminality、Claude usage continuation、Grok weekly continuation and `CredentialsPersisted` placement. Each mutation must fail the intended fixed-label owner with non-zero test exit, preserve unrelated owners, restore exact source bytes／hash／diff fingerprint, and leave the same owner green after restore. Compile failure、zero selected tests、an unrelated panic or a failure scan that ignores process exit is not a kill.

> **Windows secure-storage ownership：** 自 #276 起由 Syrtis-Windows 擁有（[Syrtis-Windows#147](https://github.com/Nanako0129/Syrtis-Windows/issues/147)）。本 repo 的 `crates/tb_core_ffi` 從未在任何 CI 或發版路徑被編譯為 Windows 目標，因此刪除了全部 Windows-only 程式碼；低階 primitive、它們的 threat boundary（trusted per-user anchor、same-SID 不在防禦範圍）與測試，都以 Syrtis-Windows 那份為準。

## Cross-port fixture cross-check

Windows port（[Nanako0129/TokenBar-Windows](https://github.com/Nanako0129/TokenBar-Windows)）的 C# `Syrtis.Core` 是 `Sources/TokenBarCore` 的逐檔移植。Reviewed pin、每次 pin 推進對這個消費者的影響（`crates/tb_core_ffi`、`ctb.h`、parser 與快取）以及歷次推進的內容，都寫在 [`architecture.md` 的 Windows downstream consumer](architecture.md#windows-downstream-consumer)，這裡不重述。

Windows 的 crosscheck oracle 釘在一個固定的 macOS commit（不是 `main`；值由 Windows repo 的 CI 設定擁有，Native 不重述），所以 Native 的 pin 推進不影響該 job；但 oracle 自身的落後因此加大，未來推進 Windows pin 時會一次掀出期間累積的漂移。same-pin 只證明 shared source 相同，不取代下述 cross-check 這道跨語言 gate；app-owned FFI、Swift 與 C# surfaces 仍可獨立漂移。單元測試的期望值由移植者撰寫，因此對「一致地誤讀 Swift 語意」的移植錯誤沒有偵測力；對拍（cross-check）以同一份 fixture JSON 餵 Swift 與 C# 兩邊、逐欄位 diff 輸出，才是移植忠實度的判準。

| 項目 | 內容 |
|---|---|
| Swift harness | [`Sources/CrossCheckHarness/main.swift`](../../Sources/CrossCheckHarness/main.swift)，`TZ=Asia/Taipei swift run crosscheck-harness <fixtures> <out> [usage-pace|format|provider-quota-pace-v3] -AppleLanguages "(en)"`；selector 省略時維持 legacy complete run，所有路徑使用 shipping 程式碼 |
| 語系鎖定 | `Format` 的日期／相對時間與 `UsagePace.durationText` 自 i18n 後具語系相依（查表工具在 `Sources/TokenBarCore/Localization.swift`，harness 透過連結 TokenBarCore 取得）。`make build` 會把 `Sources/Syrtis/Resources/Localizations/*.lproj` 複製到 `.build/debug/`；裸 `swift run Syrtis` 會在入口從 SwiftPM resource bundle 暫存同樣的目錄，harness 本身沒有該 target resource，沒有 `.lproj` 時解析為 `en`。對拍因此**必須**帶 `-AppleLanguages "(en)"`。harness 對此 **fail closed**：解析時只認 `-AppleLanguages <value>`（其餘 dash 開頭的 token 一律拒絕，避免吃掉 fixture 路徑後對錯誤目錄執行），並在 TZ guard 之後檢查 `preferredLocalizations` 必須為 `en`，否則 exit——寧可拒跑，也不要靜默產生對拍永遠對不上的在地化輸出。 |
| 契約與 fixture | Windows repo 的 legacy `crosscheck/` 保留既有 116-case reference；provider v3 handoff 由 Mac-owned [`provider-quota-pace-v3.json`](../../Fixtures/CrossCheck/provider-quota-pace-v3.json) 提供，Rust production serializer 鎖定 payload，Swift／Windows 都必須用 production decoder，無自製 wire mapping |
| 比對 | Windows repo 的 `crosscheck/diff.py`：字串逐 byte、數字 epsilon 1e-9、缺鍵視同 null |
| 執行時機 | `Sources/TokenBarCore` 邏輯或 `Format` 語意變更後；Windows app-owned delta 或 reviewed engine pin advance 後 |
| 不在對拍內：重複 label 的 qualification | 刻意由兩平台各自涵蓋，不進 Swift↔C# 對拍（[#302](https://github.com/Nanako0129/syrtis/issues/302)）。見下方說明 |

**重複 label 的 window qualification 不在對拍範圍內，這是決定，不是缺口。** 同一張卡裡重複的 window label（例如兩列 `Codex Spark`）由 `AgentUsageSnapshot.uniqueCardWindows` 在卡片呈現層加上期間、reset 或序號區分（[`Sources/TokenBarCore/AgentUsage.swift`](../../Sources/TokenBarCore/AgentUsage.swift):639，分層邏輯在 `qualifyingRepeatedLabels`，:683）。harness 的 `lifecycleRows`（[`Sources/CrossCheckHarness/main.swift`](../../Sources/CrossCheckHarness/main.swift):292）只序列化原始 `agent.windows`，所以不論 Windows 是否做了同樣的分層，對拍都是綠的。共用契約是引擎 payload，qualification 疊在它上面、屬於各平台的卡片呈現，因此兩平台各自涵蓋同一個使用者看得到的性質：macOS 由 [`Sources/Syrtis/SelfTest.swift`](../../Sources/Syrtis/SelfTest.swift) 的斷言涵蓋——:4450 起「a repeated whitelisted label is qualified in the card view」、:5211 起「repeated window label is qualified by duration, a unique one is left alone」與同組的 identity／legacy-label migration 斷言、:5306「windows whose lengths render identically still get unique names」；Windows 由它自己 repo 的 `AgentUsageQualifierTests` 涵蓋。要把 qualification 改成共用契約，就得同時移植分層、把 qualified label 加進 harness 輸出並推進 Windows pin，缺一邊都會單邊變紅。

> 首輪實績（2026-07-16）：首跑 115 案例抓到 4 條 printf 捨入 seam 的真實漂移——C# 側以 `Math.Round` 預捨入模擬 `%.nf` 會把非 midpoint 的近半值重新量化；printf 對二進位真值做正確捨入。教訓：**模擬 printf 的中介捨入層一律可疑**。修正與後續 comparator 強化（整數精確比對、bool 嚴格比對、Int64 邊界案例——fixture 現為 116 案）都記錄在 Windows repo。

> Historical pace v2 checkpoint（2026-07-16）：116-case legacy baseline 已重跑，非 historical cases 全數一致。27 個 field differences 只分布在 9 個使用舊 top-level historical scalars 的 cases：`historical-expected-clamped`、`historical-runout-exact-half`、`historical-runout-high-keeps-eta`、`historical-runout-low-forces-lasts`、`historical-with-expected`、`runout-risk-certain`、`runout-risk-clamped-above-one`、`runout-risk-half-percent-rounds-up`、`runout-risk-thirty`。這些是 nested contract 取代 scalar contract 的 intended mismatch；Windows 新增 nested fixture／DTO 並完成 semantic port 前，不得宣稱 historical parity。
>
> Provider-wide v3 checkpoint（2026-07-27）：no-selector Mac harness 以 production decoder 完整產生 42 pace＋74 format cases，不再因單一 malformed legacy row 中止。原本 28／42 pace cases 的 intended mismatch 來自 strict v3 decoder、typed lifecycle 與 no-silent-fallback contract。M19-B1 的 Windows port在 [Windows PR #7](https://github.com/Nanako0129/TokenBar-Windows/pull/7) 完成 production DTO／state machine／selection／presentation；完整 Swift／C# cross-check 119 cases零material difference。Mac-owned fixture 的7張 lifecycle windows與12個 projection／selection／legacy／malformed cases仍由Rust production serializer鎖定，real ARM64 CrossCheck也產生exact 12 cases。Provider-v3 Windows status因此為 **port/parity complete**。

> Shared-engine extraction checkpoint（2026-07-28）：Native [PR #114](https://github.com/Nanako0129/syrtis/pull/114) 與 Windows [PR #12](https://github.com/Nanako0129/TokenBar-Windows/pull/12) 都 pin `b31e39425859393504a2d56cb5af7c93e6461c7d`。Windows current-head gates produced 119 cross-check cases with zero material difference；hosted x64／ARM64 jobs and a separate native ARM64 packaged-FFI run passed. Future same-pin assertions prove shared source equality but do not replace this cross-language gate.

## Documentation checks

The knowledge tree is validated by `scripts/check_knowledge.py`, the `make check-docs` target, and the CI knowledge-validation step. The final documentation gate is:

```bash
python3 scripts/check_knowledge.py --self-test
python3 -m py_compile scripts/check_knowledge.py
python3 scripts/check_knowledge.py
make check-docs
git diff --check origin/main...HEAD
```

These checks cover frontmatter, relative links, canonical reachability, migration-ledger counts and enums, privacy scans, and repository whitespace. Do not claim runtime PASS for a docs-only change.

The owned-engine checks run without a separate engine checkout. Repository-diff checks answer "cannot ask" rather than fail when history is missing; `ci.yml` fetches this repository's history before its knowledge step, while Pages may skip those history-dependent questions.

| Check | Rule |
|---|---|
| Historical provenance | Known inherited ledger references remain checkable against `crates/syrtis_core/UPSTREAM.md`; external commit citations are not current engine pins |
| `last_verified` moves | A document whose body or `sources` changed since the merge base with `origin/main` must change `last_verified`, unless the old date is already on or after the merge base's commit date, less one day for timezones |

The engine is ordinary source, so checks do not fetch or inspect a submodule, require a reviewed pin, or demand separate engine publication. `check_engine_history` verifies inherited ledger availability and revision-label agreement; external SHA citations remain historical references. Cargo metadata and the actual native build verify dependency ownership instead of source-text snapshots. Frontmatter, relative links, privacy, migration-ledger structure, and `last_verified` checks still apply.

## Failure interpretation

A failed smoke run caused by missing local credentials, an empty private session tree, or a provider network response is not evidence that the parser or docs are wrong. Record the environmental limitation separately, then rely on hermetic tests and the relevant source-level gate. Conversely, a green live smoke run without a fixture does not close a data-dependent correctness issue.

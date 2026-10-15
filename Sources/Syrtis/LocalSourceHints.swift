import CoreServices
import Darwin
import Foundation
import TokenBarCore

/// Advisory only: native known-source checks and periodic discovery remain authoritative.
@MainActor
final class LocalSourceHints {
    enum State: String, Sendable { case stopped, watching, partial, fallback }
    enum Failure: String, Sendable {
        case unsupportedPaths, streamCreation, streamStart, eventPath
    }
    struct Diagnostics: Sendable {
        var state: State = .stopped
        var watchedRootCount = 0
        var unsupportedRootCount = 0
        var unsupportedIgnoredPathCount = 0
        var failure: Failure?
        var isWatching: Bool { state == .watching || state == .partial }
        var requiresVerificationFallback: Bool { state == .partial || state == .fallback }
    }

    private(set) var diagnostics = Diagnostics()
    private var epoch: UInt64 = 0
    private var stream: Stream?
    private var relay: Relay?
    private let queue = DispatchQueue(label: "Syrtis.LocalSourceHints", qos: .utility)

    init() {}

    func install(
        _ manifest: LocalSourceWatchManifest,
        onDirty: @escaping @MainActor @Sendable (_ requiresTopology: Bool, _ changedFileURLs: [String]?) -> Void
    ) {
        stop()
        let installedEpoch = epoch
        let filter = Filter(manifest)
        diagnostics = Diagnostics(
            state: .fallback,
            unsupportedRootCount: filter.unsupportedRoots,
            unsupportedIgnoredPathCount: filter.unsupportedIgnored,
            failure: filter.hasUnsupportedPaths ? .unsupportedPaths : nil
        )
        guard !filter.anchors.isEmpty else {
            // An empty manifest needs no stream; invalid roots must not disappear silently.
            if filter.hasUnsupportedPaths { onDirty(true, nil) }
            else { diagnostics.state = .stopped }
            return
        }
        let relay = Relay(filter: filter) { [weak self] hint in
            guard let self, self.epoch == installedEpoch else { return }
            if hint.invalidPath {
                self.diagnostics.state = .partial
                self.diagnostics.failure = .eventPath
            }
            // Topology/overflow hints verify authoritatively; only safely
            // bounded file mutations travel as named evidence, never trusted.
            onDirty(hint.topology, hint.topology ? nil : hint.changedFileURLs)
        }
        self.relay = relay
        var context = FSEventStreamContext(
            version: 0,
            info: Unmanaged.passUnretained(relay).toOpaque(),
            retain: { @Sendable pointer in
                guard let pointer else { return nil }
                _ = Unmanaged<Relay>.fromOpaque(pointer).retain()
                return pointer
            },
            release: { @Sendable pointer in
                guard let pointer else { return }
                Unmanaged<Relay>.fromOpaque(pointer).release()
            },
            copyDescription: nil
        )
        let flags = FSEventStreamCreateFlags(
            kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagWatchRoot
                | kFSEventStreamCreateFlagNoDefer
        )
        guard let created = FSEventStreamCreate(
            kCFAllocatorDefault,
            { @Sendable _, info, count, rawPaths, flags, _ in
                guard let info else { return }
                let relay = Unmanaged<Relay>.fromOpaque(info).takeUnretainedValue()
                let paths = rawPaths.assumingMemoryBound(to: UnsafePointer<CChar>.self)
                var hint: Hint?
                for index in 0..<count {
                    let eventHint = relay.filter.hint(
                        forNativeCString: paths[index], flags: flags[index]
                    )
                    if let eventHint {
                        if hint == nil { hint = eventHint }
                        else { hint!.merge(eventHint) }
                    }
                }
                if let hint { relay.enqueue(hint) }
            },
            &context, filter.anchors.map(\.string) as CFArray,
            FSEventStreamEventId(kFSEventStreamEventIdSinceNow), 0.1, flags
        ) else {
            relay.cancel()
            self.relay = nil
            diagnostics.failure = .streamCreation
            onDirty(true, nil)
            return
        }
        FSEventStreamSetDispatchQueue(created, queue)
        guard FSEventStreamStart(created) else {
            relay.cancel()
            FSEventStreamInvalidate(created)
            FSEventStreamRelease(created)
            self.relay = nil
            diagnostics.failure = .streamStart
            onDirty(true, nil)
            return
        }
        stream = Stream(created)
        diagnostics.watchedRootCount = filter.anchors.count
        diagnostics.state = filter.hasUnsupportedPaths ? .partial : .watching
        if filter.hasUnsupportedPaths { onDirty(true, nil) }
    }

    func stop() {
        epoch &+= 1
        relay?.cancel()
        relay = nil
        stream?.stop()
        stream = nil
        diagnostics = Diagnostics()
    }

    deinit {
        // FSEvents retains Relay, never this owner. Stream's deinit releases native resources.
        relay?.cancel()
    }

    // The handle is confined to the main actor until this private holder's final release;
    // callbacks only use Relay. The Sendable holder permits safe nonisolated owner deinit.
    private final class Stream: @unchecked Sendable {
        private var handle: FSEventStreamRef?
        init(_ handle: FSEventStreamRef) { self.handle = handle }
        func stop() {
            guard let handle else { return }
            self.handle = nil
            FSEventStreamStop(handle)
            FSEventStreamInvalidate(handle)
            FSEventStreamRelease(handle)
        }
        deinit { stop() }
    }

    static func selfTest(_ expect: (Bool, String) -> Void) {
        let json = #"""
        {"roots":[
          {"sourceRootURL":"file:///fixture/home/.agent/sessions","watchRootURL":"file:///fixture/home","isDirectory":true},
          {"sourceRootURL":"file:///fixture/db/usage.db","watchRootURL":"file:///fixture/db","isDirectory":false},
          {"sourceRootURL":"file:///fixture/db/usage.db-wal","watchRootURL":"file:///fixture/db","isDirectory":false},
          {"sourceRootURL":"file:///fixture/%FF/log","watchRootURL":"file:///fixture","isDirectory":true}
        ],"ignoredFileURLs":["file:///fixture/home/.agent/sessions/owned.db",
          "file:///fixture/home/.agent/sessions/owned.db-wal"],
          "ignoredBasenameRules":[
            {"parentURL":"file:///fixture/home/.agent/sessions",
             "pattern":"^\\.dashboard-snapshot\\.json\\.[0-9]+\\.[0-9]+\\.tmp$"},
            {"parentURL":"file:///fixture/home/.agent/sessions",
             "pattern":"^\\.usage-restore-[0-9]+-[0-9]+\\.sqlite3(?:-wal|-shm|-journal)?$"},
            {"parentURL":"file:///fixture/home/.agent/sessions",
             "pattern":"^\\.pricing-(?:litellm|openrouter)\\.json\\.[0-9]+\\.[0-9a-f]+\\.tmp$"},
            {"parentURL":"file:///fixture/home/.agent/sessions",
             "pattern":"^\\.opencode-migration\\.[0-9]+\\.[0-9a-f]+\\.tmp$"},
            {"parentURL":"file:///fixture/home/.agent/sessions","pattern":"owned.*"},
            {"parentURL":"file:///fixture/%FF","pattern":"^owned$"}]}
        """#
        guard let manifest = try? JSONDecoder().decode(
            LocalSourceWatchManifest.self, from: Data(json.utf8)
        ) else { expect(false, "source hints fixture decodes"); return }
        let filter = Filter(manifest)
        let modified = UInt32(kFSEventStreamEventFlagItemModified | kFSEventStreamEventFlagItemIsFile)
        let created = UInt32(kFSEventStreamEventFlagItemCreated | kFSEventStreamEventFlagItemIsDir)
        func hint(_ string: String, _ flags: UInt32? = nil) -> Hint? {
            guard let path = NativePath(string) else { return nil }
            return filter.hint(for: path, flags: flags ?? modified)
        }
        expect(filter.unsupportedRoots == 1 && filter.anchors.count == 2,
               "invalid UTF8 root retains explicit fallback; valid anchors deduplicate")
        let nestedJSON = #"""
        {"roots":[
          {"sourceRootURL":"file:///fixture/home/.agent/sessions","watchRootURL":"file:///fixture/home/.agent","isDirectory":true},
          {"sourceRootURL":"file:///fixture/home/.missing/sessions","watchRootURL":"file:///fixture/home","isDirectory":true},
          {"sourceRootURL":"file:///fixture/home-other/logs","watchRootURL":"file:///fixture/home-other","isDirectory":true}
        ],"ignoredFileURLs":[],"ignoredBasenameRules":[]}
        """#
        if let nestedManifest = try? JSONDecoder().decode(
            LocalSourceWatchManifest.self, from: Data(nestedJSON.utf8)
        ) {
            let nested = Filter(nestedManifest)
            expect(nested.anchors.map(\.string) == ["/fixture/home", "/fixture/home-other"],
                   "recursive watches collapse covered anchors without collapsing prefix siblings")
            expect(nested.hint(for: NativePath("/fixture/home/.agent/sessions/new/log")!, flags: created)?.topology == true
                   && nested.hint(for: NativePath("/fixture/home/.missing")!, flags: created)?.topology == true
                   && nested.hint(for: NativePath("/fixture/home/unrelated")!, flags: created) == nil,
                   "collapsed watches preserve exact source events, recovery and unrelated-sibling filtering")
        } else { expect(false, "nested watch manifest decodes") }
        resourceSelfTest(expect)
        expect(hint("/fixture/home/unrelated/file") == nil,
               "broad recovery anchor rejects unrelated home events")
        expect(hint("/fixture/home/.agent/sessions-other/file") == nil,
               "source prefix is component exact")
        expect(hint("/fixture/home/.agent/sessions/log")?.topology == false,
               "scoped content mutation is a known-source hint")
        expect(hint("/fixture/home/.agent", created)?.topology == true,
               "nearest-parent recovery requests topology")
        expect(hint("/fixture/home/.other", created) == nil,
               "nearest-parent recovery excludes sibling topology")
        expect(hint("/fixture/home/.agent/sessions/owned.db") == nil
               && hint("/fixture/home/.agent/sessions/owned.db-wal") == nil,
               "exact owned output and WAL writes are ignored")
        expect(hint("/fixture/home/.agent/sessions/cache-input.json") != nil,
               "cache-named source inputs are not blanket ignored")
        expect(hint("/fixture/home/.agent/sessions/.dashboard-snapshot.json.12.345.tmp") == nil
               && hint("/fixture/home/.agent/sessions/nested/.dashboard-snapshot.json.12.345.tmp") != nil
               && hint("/fixture/home/.agent/sessions/.dashboard-snapshot.json.12.345.tmp-source") != nil
               && hint("/fixture/home/.agent/sessions/.dashboard-snapshot.json.12.345.tmp\n") != nil,
               "future staging writes match complete basename at only the exact owned parent")
        expect(hint("/fixture/home/.agent/sessions/.usage-restore-12-345.sqlite3-wal") == nil
               && hint("/fixture/home/.agent/sessions/.pricing-litellm.json.12.abc.tmp") == nil
               && hint("/fixture/home/.agent/sessions/.pricing-openrouter.json.12.abc.tmp") == nil
               && hint("/fixture/home/.agent/sessions/.opencode-migration.12.abc.tmp") == nil
               && hint("/fixture/home/.agent/sessions/.pricing-litellm.json.12.xyz.tmp") != nil,
               "native recovery, pricing and migration writer grammars preserve exact exclusions")
        expect(filter.unsupportedIgnored == 2
               && hint("/fixture/home/.agent/sessions/owned-source.json") != nil,
               "unsupported ignore rules request fallback, never widen source exclusions")
        expect(hint("/fixture/db/usage.db-wal")?.topology == false
               && hint("/fixture/db", UInt32(kFSEventStreamEventFlagItemIsDir)) != nil
               && hint("/fixture/db/unrelated.db") == nil,
               "DB WAL and exact coarse parent events retain file scope")
        expect(hint("/fixture/home", UInt32(kFSEventStreamEventFlagRootChanged))?.topology == true
               && hint("/fixture", UInt32(kFSEventStreamEventFlagUnmount))?.topology == true
               && hint("/unrelated", UInt32(kFSEventStreamEventFlagMustScanSubDirs))?.topology == true,
               "root change, unmount, and event loss require authoritative discovery")
        expect(NativePath(fileURL: "file:///fixture/%FF") == nil
               && NativePath(fileURL: "https://host/fixture") == nil
               && NativePath(fileURL: "file:///fixture/%00bad") == nil,
               "invalid native paths never become lossy or non-file watches")
        expect(NativePath("/fixture/\u{00E9}") != NativePath("/fixture/e\u{0301}"),
               "Unicode-equivalent native byte identities remain distinct")
        func invalidNativeHint(_ prefix: String) -> Hint? {
            let bytes = (Array(prefix.utf8) + [255, 0]).map { CChar(bitPattern: $0) }
            return bytes.withUnsafeBufferPointer {
                filter.hint(forNativeCString: $0.baseAddress!, flags: modified)
            }
        }
        expect(invalidNativeHint("/fixture/home/unrelated/") == nil
               && invalidNativeHint("/fixture/home/.agent/sessions/")?.invalidPath == true,
               "raw invalid UTF8 events only request fallback inside exact source scopes")
        let plainFlags = UInt32(kFSEventStreamEventFlagItemModified | kFSEventStreamEventFlagItemIsFile)
        let structuralFlags = UInt32(kFSEventStreamEventFlagItemRenamed | kFSEventStreamEventFlagItemIsFile)
        expect(hint("/fixture/home/.agent/sessions/log", plainFlags)?.changedFileURLs
               == ["file:///fixture/home/.agent/sessions/log"],
               "plain regular-file mutation names its exact evidence")
        expect(hint("/fixture/home/.agent/sessions/log", structuralFlags)?.changedFileURLs == nil
               && hint("/fixture/home/.agent/sessions/log", structuralFlags)?.topology == true,
               "rename stays flag-only so verification owns the new identity")
        expect(hint("/fixture/db", UInt32(kFSEventStreamEventFlagItemIsDir))?.changedFileURLs == nil,
               "coarse parent hints stay flag-only and never name a child")
        var coalesced = hint("/fixture/home/.agent/sessions/a", plainFlags)!
        coalesced.merge(hint("/fixture/home/.agent/sessions/b", plainFlags)!)
        expect(coalesced.topology == false && coalesced.changedFileURLs?.count == 2,
               "coalescing keeps bounded named evidence with flag priority")
        let unnamed = hint("/fixture/db", UInt32(kFSEventStreamEventFlagItemIsDir))!
        var mixed = coalesced
        mixed.merge(unnamed)
        mixed.merge(coalesced)
        expect(mixed.changedFileURLs == nil,
               "coarse changes cannot be lost beside later named file changes")
        var reverse = unnamed
        reverse.merge(coalesced)
        expect(reverse.changedFileURLs == nil,
               "coalescing preserves unnamed verification in either event order")
        var overflowed = Hint(topology: false, paths: ["file:///fixture/home/.agent/sessions/0"])
        for index in 1..<300 {
            overflowed.merge(Hint(topology: false, paths: ["file:///fixture/home/.agent/sessions/\(index)"]))
        }
        expect(overflowed.topology == false && overflowed.changedFileURLs == nil,
               "overflowing named evidence falls back to authoritative verification")
        Relay.selfTest(filter: filter, expect: expect)
    }

    private static func resourceSelfTest(_ expect: (Bool, String) -> Void) {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("syrtis-hints-\(UUID().uuidString)", isDirectory: true)
        let watcher = LocalSourceHints()
        defer {
            watcher.stop()
            try? FileManager.default.removeItem(at: directory)
        }
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            var roots = [LocalSourceWatchRoot(
                sourceRootURL: directory.absoluteString,
                watchRootURL: directory.absoluteString, isDirectory: true)]
            for index in 0..<40 {
                let child = directory.appendingPathComponent("client-\(index)/sessions", isDirectory: true)
                try FileManager.default.createDirectory(at: child, withIntermediateDirectories: true)
                roots.append(.init(sourceRootURL: child.absoluteString,
                                   watchRootURL: child.absoluteString, isDirectory: true))
            }
            let manifest = LocalSourceWatchManifest(roots: roots, ignoredFileURLs: [])
            func descriptors() -> Int {
                (0..<4096).reduce(0) { $0 + (fcntl(Int32($1), F_GETFD) >= 0 ? 1 : 0) }
            }
            let baseline = descriptors()
            for _ in 0..<3 {
                watcher.install(manifest) { _, _ in }
                Thread.sleep(forTimeInterval: 0.1)
                expect(watcher.diagnostics.isWatching && descriptors() - baseline < 32,
                       "overlapping native watches leave descriptor headroom for source reads")
                watcher.stop()
                Thread.sleep(forTimeInterval: 0.1)
                expect(descriptors() - baseline < 8,
                       "replaced native watches release their directory handles")
            }
        } catch { expect(false, "native watch resource fixture: \(error)") }
    }

    /// Byte equality avoids Swift String's canonical-Unicode-equivalence path collisions.
    private struct NativePath: Hashable, Sendable {
        let bytes: [UInt8]
        // Only validated manifest paths are converted to CFStrings for stream creation.
        var string: String { String(bytes: bytes, encoding: .utf8)! }

        init?(_ string: String) {
            var bytes = Array(string.utf8)
            guard bytes.first == 47, !bytes.contains(0) else { return nil }
            while bytes.count > 1 && bytes.last == 47 { bytes.removeLast() }
            let components = bytes.split(separator: 47, omittingEmptySubsequences: false)
            guard !components.dropFirst().contains(where: {
                $0.isEmpty || $0.elementsEqual([46]) || $0.elementsEqual([46, 46])
            }) || bytes == [47] else { return nil }
            self.bytes = bytes
        }

        init?(nativeCString pointer: UnsafePointer<CChar>) {
            var bytes = Array(UnsafeRawBufferPointer(start: pointer, count: strlen(pointer)))
            guard bytes.first == 47 else { return nil }
            while bytes.count > 1 && bytes.last == 47 { bytes.removeLast() }
            self.bytes = bytes
        }

        init?(fileURL string: String) {
            guard let url = URLComponents(string: string), url.scheme == "file",
                  url.host == nil || url.host == "" || url.host == "localhost",
                  url.user == nil, url.password == nil, url.port == nil,
                  url.query == nil, url.fragment == nil,
                  let path = url.percentEncodedPath.removingPercentEncoding else { return nil }
            self.init(path)
        }

        func contains(_ other: NativePath) -> Bool {
            self == other || (other.bytes.starts(with: bytes)
                && (bytes == [47] || other.bytes.dropFirst(bytes.count).first == 47))
        }

        var parent: NativePath? {
            guard bytes.count > 1, let slash = bytes.lastIndex(of: 47) else { return nil }
            guard let string = String(bytes: bytes.prefix(max(1, slash)), encoding: .utf8)
            else { return nil }
            return NativePath(string)
        }
    }

    private struct Hint: Sendable {
        var topology: Bool
        var invalidPath = false
        /// Exact admitted regular-file URLs. Always re-verified, never trusted.
        var paths: Set<String> = []
        private var overflowed = false
        /// Sorted for deterministic delivery; nil when flag-only or overflowed.
        var changedFileURLs: [String]? { overflowed || paths.isEmpty ? nil : paths.sorted() }
        mutating func merge(_ other: Hint) {
            topology = topology || other.topology
            invalidPath = invalidPath || other.invalidPath
            overflowed = overflowed || other.overflowed || paths.isEmpty || other.paths.isEmpty
            // Overflowing named evidence falls back to authoritative
            // verification for the whole batch; the merged flags keep the
            // stronger fallback and later names cannot resurrect it.
            if !overflowed {
                paths.formUnion(other.paths)
                if paths.count > Self.maxPaths { overflowed = true; paths.removeAll() }
            }
        }
        // ponytail: fixed 256-path bound; raise only with measured Rust verify wins.
        private static let maxPaths = 256
    }

    private struct Filter: Sendable {
        struct Root: Sendable {
            let source: NativePath
            let anchor: NativePath
            let isDirectory: Bool
        }
        // NSRegularExpression is immutable and supports concurrent matching. Parent identity
        // remains native bytes; a rule can only ignore one basename in that exact directory.
        struct OwnedWriteRule: @unchecked Sendable {
            let parent: NativePath
            let expression: NSRegularExpression

            func matches(_ path: NativePath) -> Bool {
                guard let slash = path.bytes.lastIndex(of: 47),
                      path.bytes.prefix(max(1, slash)).elementsEqual(parent.bytes),
                      let basename = String(bytes: path.bytes.dropFirst(slash + 1), encoding: .utf8)
                else { return false }
                let range = NSRange(location: 0, length: basename.utf16.count)
                return expression.firstMatch(in: basename, range: range)?.range == range
            }
        }
        let roots: [Root]
        let anchors: [NativePath]
        let ignored: Set<NativePath>
        let ignoredRules: [OwnedWriteRule]
        let unsupportedRoots: Int
        let unsupportedIgnored: Int
        var hasUnsupportedPaths: Bool { unsupportedRoots > 0 || unsupportedIgnored > 0 }

        init(_ manifest: LocalSourceWatchManifest) {
            var roots: [Root] = []
            var unsupportedRoots = 0
            for root in manifest.roots {
                guard let source = NativePath(fileURL: root.sourceRootURL),
                      let anchor = NativePath(fileURL: root.watchRootURL),
                      anchor.contains(source) else {
                    unsupportedRoots += 1
                    continue
                }
                roots.append(Root(source: source, anchor: anchor, isDirectory: root.isDirectory))
            }
            self.roots = roots
            // WatchRoot opens each anchor's ancestor chain. FSEvents watches recursively,
            // so covered descendants only duplicate handles, not event coverage.
            var anchors: [NativePath] = []
            for anchor in Set(roots.map(\.anchor)).sorted(by: {
                $0.bytes.lexicographicallyPrecedes($1.bytes)
            }) where !anchors.contains(where: { $0.contains(anchor) }) {
                anchors.append(anchor)
            }
            self.anchors = anchors
            self.unsupportedRoots = unsupportedRoots
            var ignored: Set<NativePath> = []
            var unsupportedIgnored = 0
            for url in manifest.ignoredFileURLs {
                if let path = NativePath(fileURL: url) { ignored.insert(path) }
                else { unsupportedIgnored += 1 }
            }
            var ignoredRules: [OwnedWriteRule] = []
            for rule in manifest.ignoredBasenameRules {
                guard let parent = NativePath(fileURL: rule.parentURL),
                      rule.pattern.hasPrefix("^"), rule.pattern.hasSuffix("$"),
                      let expression = try? NSRegularExpression(pattern: rule.pattern) else {
                    unsupportedIgnored += 1
                    continue
                }
                ignoredRules.append(OwnedWriteRule(parent: parent, expression: expression))
            }
            // Native writer grammars cover future staging names, never entire cache dirs.
            self.ignored = ignored
            self.ignoredRules = ignoredRules
            self.unsupportedIgnored = unsupportedIgnored
        }

        func hint(
            forNativeCString pointer: UnsafePointer<CChar>,
            flags: FSEventStreamEventFlags
        ) -> Hint? {
            guard let path = NativePath(nativeCString: pointer)
            else { return Hint(topology: true, invalidPath: true) }
            // Filter raw bytes first: invalid UTF8 in an unrelated home sibling must not
            // dirty our owner. Relevant invalid paths request native verification instead.
            guard var hint = hint(for: path, flags: flags) else { return nil }
            if String(bytes: path.bytes, encoding: .utf8) == nil {
                hint.topology = true
                hint.invalidPath = true
            }
            return hint
        }

        func hint(for path: NativePath, flags: FSEventStreamEventFlags) -> Hint? {
            let overflow = UInt32(kFSEventStreamEventFlagMustScanSubDirs
                | kFSEventStreamEventFlagUserDropped | kFSEventStreamEventFlagKernelDropped
                | kFSEventStreamEventFlagEventIdsWrapped)
            if flags & overflow != 0 { return Hint(topology: true) }
            let rootChange = UInt32(kFSEventStreamEventFlagRootChanged
                | kFSEventStreamEventFlagMount | kFSEventStreamEventFlagUnmount)
            if flags & rootChange != 0 {
                return roots.contains(where: {
                    path.contains($0.source) || $0.source.contains(path)
                        || path == $0.anchor
                }) ? Hint(topology: true) : nil
            }
            guard !ignored.contains(path), !ignoredRules.contains(where: { $0.matches(path) })
            else { return nil }
            let structural = UInt32(kFSEventStreamEventFlagItemCreated
                | kFSEventStreamEventFlagItemRemoved | kFSEventStreamEventFlagItemRenamed
                | kFSEventStreamEventFlagItemIsSymlink)
            // Only a plain regular-file modification names evidence; created,
            // deleted, renamed, directory, and coarse-parent events stay
            // flag-only so Rust verifies authoritatively instead of trusting them.
            let namesFile = flags & UInt32(kFSEventStreamEventFlagItemIsFile) != 0
                && flags & UInt32(kFSEventStreamEventFlagItemIsDir) == 0
                && flags & structural == 0
            func fileHint() -> Hint {
                guard namesFile, let url = Self.fileURLString(for: path) else { return Hint(topology: false) }
                return Hint(topology: false, paths: [url])
            }
            for root in roots {
                if path == root.source {
                    if flags & structural != 0 { return Hint(topology: true) }
                    // A replaced file root is a new identity, not a mutation.
                    if !root.isDirectory { return fileHint() }
                    return Hint(topology: false)
                }
                if root.isDirectory && root.source.contains(path) {
                    if flags & structural != 0 { return Hint(topology: true) }
                    return fileHint()
                }
                // Recover a missing root only from its own ancestor chain, not siblings
                // beneath a broad nearest-existing anchor (which may be the home directory).
                if root.anchor.contains(path) && path.contains(root.source)
                    && flags & structural != 0 {
                    return Hint(topology: true)
                }
                // FileEvents can still coalesce an update into an exact DB/WAL parent.
                // This coarse hint is advisory; it cannot establish which child changed.
                if !root.isDirectory && path == root.source.parent
                    && (flags & UInt32(kFSEventStreamEventFlagItemIsDir) != 0
                        || flags & UInt32(kFSEventStreamEventFlagItemIsFile) == 0) {
                    return Hint(topology: false)
                }
            }
            return nil
        }

        /// Rust owns file-URL parsing; only validated UTF8 paths become URLs here.
        private static func fileURLString(for path: NativePath) -> String? {
            guard let string = String(bytes: path.bytes, encoding: .utf8) else { return nil }
            return URL(fileURLWithPath: string).absoluteString
        }
    }

    /// The C callback sees immutable scopes; only this bounded pending hint uses a lock.
    private final class Relay: @unchecked Sendable {
        let filter: Filter
        private let deliver: @MainActor @Sendable (Hint) -> Void
        private let lock = NSLock()
        private var active = true
        private var pending: Hint?
        private var scheduled = false

        init(filter: Filter, deliver: @escaping @MainActor @Sendable (Hint) -> Void) {
            self.filter = filter
            self.deliver = deliver
        }

        func enqueue(_ hint: Hint) {
            if stage(hint) {
                Task { @MainActor [self] in
                    if let hint = takePending() { deliver(hint) }
                }
            }
        }

        private func stage(_ hint: Hint) -> Bool {
            lock.lock()
            defer { lock.unlock() }
            guard active else { return false }
            if pending == nil { pending = hint }
            else { pending!.merge(hint) }
            let needsDelivery = !scheduled
            scheduled = true
            return needsDelivery
        }

        private func takePending() -> Hint? {
            lock.lock()
            defer { lock.unlock() }
            scheduled = false
            defer { pending = nil }
            return active ? pending : nil
        }

        func cancel() {
            lock.lock()
            active = false
            pending = nil
            lock.unlock()
        }

        @MainActor
        static func selfTest(filter: Filter, expect: (Bool, String) -> Void) {
            let relay = Relay(filter: filter) { _ in }
            expect(relay.stage(Hint(topology: false))
                   && !relay.stage(Hint(topology: true, invalidPath: true)),
                   "repeated callbacks coalesce into one pending delivery")
            let pending = relay.takePending()
            expect(pending?.topology == true && pending?.invalidPath == true,
                   "coalescing preserves topology and invalid-path priority")
            expect(relay.stage(Hint(topology: false)),
                   "a drained stream accepts its next batch")
            relay.cancel()
            expect(relay.takePending() == nil && !relay.stage(Hint(topology: true)),
                   "stopped/replaced stream drops pending and late callbacks")
        }
    }
}

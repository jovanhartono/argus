import AppKit
import DiskArbitration
import XCTest
import os

@testable import Argus

@MainActor
final class DrivesTests: XCTestCase {
    private let center = NotificationCenter()
    private var heard: [String] = []
    private var stillMounted: Set<String> = []

    private func drives(mounted: [String] = []) -> Drives {
        Drives(
            center: center, mounted: mounted, isMounted: { [unowned self] in stillMounted.contains($0) },
            settle: .milliseconds(50), added: { [unowned self] in heard.append("+" + $0) },
            removing: { [unowned self] in heard.append("-" + $0) })
    }

    private func post(_ name: Notification.Name, _ path: String?, old: String? = nil) {
        var info: [AnyHashable: Any] = [:]
        info[NSWorkspace.volumeURLUserInfoKey] = path.map { URL(fileURLWithPath: $0, isDirectory: true) }
        info[NSWorkspace.oldVolumeURLUserInfoKey] = old.map { URL(fileURLWithPath: $0, isDirectory: true) }
        center.post(name: name, object: NSWorkspace.shared, userInfo: info)
    }

    /// Past the settle time given above.
    private func settled() async throws {
        try await Task.sleep(for: .milliseconds(200))
    }

    func testVolumesMountedAtLaunchArePassedOnFirstInOrder() {
        let drives = drives(mounted: ["/", "/Volumes/Photos", "/Volumes/Backup"])
        XCTAssertEqual(heard, ["+/", "+/Volumes/Photos", "+/Volumes/Backup"])
        _ = drives
    }

    func testAPluggedInDriveIsAddedAndAnEjectedOneLetGoBeforeItUnmounts() async throws {
        let drives = drives()
        post(NSWorkspace.didMountNotification, "/Volumes/Photos")
        post(NSWorkspace.willUnmountNotification, "/Volumes/Photos")
        post(NSWorkspace.didUnmountNotification, "/Volumes/Photos")
        try await settled()
        XCTAssertEqual(heard, ["+/Volumes/Photos", "-/Volumes/Photos"], "let go of once")
        XCTAssertEqual(drives.leaving, [])
    }

    func testADrivePulledOutWithoutAnEjectIsLetGo() {
        let drives = drives()
        post(NSWorkspace.didMountNotification, "/Volumes/Photos")
        post(NSWorkspace.didUnmountNotification, "/Volumes/Photos")
        post(NSWorkspace.didMountNotification, "/Volumes/Photos")
        XCTAssertEqual(heard, ["+/Volumes/Photos", "-/Volumes/Photos", "+/Volumes/Photos"])
        _ = drives
    }

    func testEachUnmountIsToldOnceDoneWithOrWithoutAnEject() {
        var gone: [String] = []
        let drives = Drives(
            center: center, mounted: [], isMounted: { _ in false }, added: { _ in }, removing: { _ in },
            gone: { gone.append($0) })
        post(NSWorkspace.willUnmountNotification, "/Volumes/Photos")
        XCTAssertEqual(gone, [], "still mounted while macOS asks")
        post(NSWorkspace.didUnmountNotification, "/Volumes/Photos")
        post(NSWorkspace.didUnmountNotification, "/Volumes/Music")
        XCTAssertEqual(gone, ["/Volumes/Photos", "/Volumes/Music"])
        _ = drives
    }

    func testADriveStillMountedAfterAnEjectIsListedAgain() async throws {
        let drives = drives()
        stillMounted = ["/Volumes/Photos"]
        post(NSWorkspace.willUnmountNotification, "/Volumes/Photos")
        XCTAssertEqual(heard, ["-/Volumes/Photos"])
        try await settled()
        XCTAssertEqual(heard, ["-/Volumes/Photos", "+/Volumes/Photos"], "a file on it was open, so it stayed")
        XCTAssertEqual(drives.leaving, [])
    }

    func testADriveGoneWithoutAnUnmountNoteIsNotListedAgain() async throws {
        let drives = drives()
        post(NSWorkspace.willUnmountNotification, "/Volumes/Photos")
        try await settled()
        XCTAssertEqual(heard, ["-/Volumes/Photos"])
        XCTAssertEqual(drives.leaving, [])
    }

    func testADriveMountedAgainBeforeTheCheckIsListedOnce() async throws {
        let drives = drives()
        stillMounted = ["/Volumes/Photos"]
        post(NSWorkspace.willUnmountNotification, "/Volumes/Photos")
        post(NSWorkspace.didUnmountNotification, "/Volumes/Photos")
        post(NSWorkspace.didMountNotification, "/Volumes/Photos")
        try await settled()
        XCTAssertEqual(heard, ["-/Volumes/Photos", "+/Volumes/Photos"])
        _ = drives
    }

    func testMacOSAskingToUnmountLetsGoWhileTheMainThreadIsBusy() async throws {
        let gone = OSAllocatedUnfairLock<[String]>(initialState: [])
        let drives = Drives(
            center: center, mounted: [], isMounted: { _ in false }, settle: .milliseconds(50),
            letGo: { path in gone.withLock { $0.append(path) } },
            added: { [unowned self] in heard.append("+" + $0) },
            removing: { [unowned self] in heard.append("-" + $0) })
        // The main thread waits here throughout, as on a slow search.
        DispatchQueue.global().sync { drives.unmountAsked("/Volumes/Photos") }
        XCTAssertEqual(gone.withLock { $0 }, ["/Volumes/Photos"])
        XCTAssertEqual(heard, [], "the window hears once the main thread is free")
        post(NSWorkspace.willUnmountNotification, "/Volumes/Photos")
        XCTAssertEqual(heard, ["-/Volumes/Photos"])
        try await Task.sleep(for: .milliseconds(10))
        post(NSWorkspace.didUnmountNotification, "/Volumes/Photos")
        try await settled()
        XCTAssertEqual(heard, ["-/Volumes/Photos"], "let go of once")
        XCTAssertEqual(drives.leaving, [])
    }

    func testASlowUnmountIsNeverListedAgain() async throws {
        var asked = 0
        let drives = Drives(
            center: center, mounted: [],
            isMounted: { _ in
                asked += 1
                // Answered only once the unmount, still under way at the check, is done.
                try? await Task.sleep(for: .milliseconds(300))
                return false
            },
            settle: .milliseconds(50), added: { [unowned self] in heard.append("+" + $0) },
            removing: { [unowned self] in heard.append("-" + $0) })
        post(NSWorkspace.willUnmountNotification, "/Volumes/Photos")
        try await Task.sleep(for: .milliseconds(600))
        XCTAssertEqual(asked, 1)
        XCTAssertEqual(heard, ["-/Volumes/Photos"])
        XCTAssertEqual(drives.leaving, [])
    }

    func testAnEjectTriedAgainStartsTheWaitOver() async throws {
        let drives = Drives(
            center: center, mounted: [], isMounted: { [unowned self] in stillMounted.contains($0) },
            settle: .milliseconds(150), added: { [unowned self] in heard.append("+" + $0) },
            removing: { [unowned self] in heard.append("-" + $0) })
        stillMounted = ["/Volumes/Photos"]
        post(NSWorkspace.willUnmountNotification, "/Volumes/Photos")
        try await Task.sleep(for: .milliseconds(100))
        // Refused, then ejected again: macOS asks once more, and that unmount may still be on its way.
        DispatchQueue.global().sync { drives.unmountAsked("/Volumes/Photos") }
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(heard, ["-/Volumes/Photos"], "not listed again at the first eject's check")
        XCTAssertEqual(drives.leaving, ["/Volumes/Photos"])
        try await Task.sleep(for: .milliseconds(250))
        XCTAssertEqual(heard, ["-/Volumes/Photos", "+/Volumes/Photos"], "refused for good, so listed again once")
        XCTAssertEqual(drives.leaving, [])
    }

    func testOnlyAMountPointCountsAsStillMounted() async throws {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: folder) }
        let isMounted = await Drives.stillMounted("/")
        XCTAssertTrue(isMounted)
        let folderIsMounted = await Drives.stillMounted(folder.path)
        XCTAssertFalse(folderIsMounted, "a folder on the startup disk")
        let goneIsMounted = await Drives.stillMounted("/Volumes/No Such Drive")
        XCTAssertFalse(goneIsMounted)
    }

    func testARenamedDriveMovesToItsNewName() {
        let drives = drives()
        post(NSWorkspace.didRenameVolumeNotification, "/Volumes/Pics", old: "/Volumes/Photos")
        post(NSWorkspace.didRenameVolumeNotification, "/Volumes/Pics")
        XCTAssertEqual(heard, ["-/Volumes/Photos", "+/Volumes/Pics"], "no old name, nothing to move")
        _ = drives
    }

    func testNoTimerOutlivesDrives() async throws {
        var drives: Drives? = drives()
        stillMounted = ["/Volumes/Photos"]
        post(NSWorkspace.willUnmountNotification, "/Volumes/Photos")
        drives = nil
        try await settled()
        XCTAssertNil(drives)
        XCTAssertEqual(heard, ["-/Volumes/Photos"])
    }

    func testANameWithSpacesAndAccentsComesThroughWhole() {
        let drives = drives()
        post(NSWorkspace.didMountNotification, "/Volumes/Fotos de Zoë 2")
        XCTAssertEqual(heard, ["+/Volumes/Fotos de Zoë 2"])
        _ = drives
    }

    func testANoteWithoutAVolumeIsIgnored() {
        let drives = drives()
        post(NSWorkspace.didMountNotification, nil)
        center.post(
            name: NSWorkspace.willUnmountNotification, object: nil,
            userInfo: [NSWorkspace.volumeURLUserInfoKey: "/Volumes/Photos"])
        XCTAssertEqual(heard, [])
        _ = drives
    }

    func testNothingIsHeardOnceFreed() {
        var drives: Drives? = drives()
        drives = nil
        post(NSWorkspace.didMountNotification, "/Volumes/Photos")
        XCTAssertNil(drives)
        XCTAssertEqual(heard, [])
    }

    func testTheSystemListsTheStartupDisk() {
        XCTAssertTrue(Drives.mounted().contains("/"))
    }

    func testAFolderOnADriveLeadsWithTheDrivesName() {
        XCTAssertEqual(ResultCell.place("/Volumes/Photos"), "Photos")
        XCTAssertEqual(ResultCell.place("/Volumes/Photos/trips"), "Photos › trips")
        XCTAssertEqual(ResultCell.place("/Volumes/My Drive/trips/2024 summer"), "My Drive › trips/2024 summer")
        let home = "/Volumes/Data/Users/me"
        XCTAssertEqual(ResultCell.place("/Volumes/Data/Users/me/notes", home: home), "~/notes")
        XCTAssertEqual(ResultCell.place(home, home: home), "~")
        XCTAssertEqual(ResultCell.place("/Volumes/Data/Users/you", home: home), "Data › Users/you")
        XCTAssertEqual(ResultCell.place("/Users/me/notes", home: "/Users/me"), "~/notes")
        XCTAssertEqual(ResultCell.place("/Users/meg/notes", home: "/Users/me"), "/Users/meg/notes")
        XCTAssertEqual(ResultCell.place("/Users/me/notes", home: "/Users/jan"), "/Users/me/notes")
        XCTAssertEqual(ResultCell.place("~/notes"), "~/notes")
        XCTAssertEqual(ResultCell.place("/Volumes"), "/Volumes")
        XCTAssertEqual(ResultCell.place("/Volumes/"), "/Volumes/")
        XCTAssertEqual(ResultCell.place("/VolumesX/a"), "/VolumesX/a")
        XCTAssertEqual(ResultCell.place(""), "")
    }

    func testAFolderInsideTheChipsFolderIsWrittenFromThere() {
        let home = "/Users/me"
        XCTAssertEqual(ResultCell.place("/Users/me/Budget", in: "/Users/me/Budget", home: home), "Budget")
        XCTAssertEqual(
            ResultCell.place("/Users/me/Budget/Invoices/2024", in: "/Users/me/Budget", home: home), "Budget › Invoices/2024")
        XCTAssertEqual(ResultCell.place("/Users/me/Budgets", in: "/Users/me/Budget", home: home), "~/Budgets")
        XCTAssertEqual(ResultCell.place("/Volumes/Photos/trips/2024", in: "/Volumes/Photos/trips", home: home), "trips › 2024")
    }

    func testARowOnADriveShowsTheDrivesName() {
        let cell = ResultCell()
        cell.show(Row(name: "beach.jpg", folder: "/Volumes/Photos/trips", isFolder: false), icon: NSImage())
        XCTAssertEqual(cell.folder.stringValue, "Photos › trips")
        let hit = TextHit(
            path: "/Volumes/Photos/notes/plan.txt", text: "plan", match: NSRange(location: 0, length: 4), line: 3)
        cell.show(hit.file, icon: NSImage())
        XCTAssertEqual(cell.folder.stringValue, "Photos › notes", "a file with lines found inside")
    }

    func testTheWindowLeavesOutWhatTheCoreRefuses() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let window = SearchPanel(root: root.path, hasAccess: { true }, stage: OffStage())
        for _ in 0..<500 where !window.indexed { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(window.indexed)
        for path in ["/", "/Volumes/No Such Drive", root.path] {
            window.driveMounted(path)
            window.driveUnmounting(path)
        }
        XCTAssertEqual(window.drivesTold, [:], "no list follows, so nothing waits to be timed")
    }

    func testADriveIsTimedOnlyOnceAListHoldingItIsInUse() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let window = SearchPanel(root: root.path, hasAccess: { true }, stage: OffStage())
        window.drivesTold = ["/Volumes/Photos": 1, "/Volumes/Music": 2]
        // A change at home lands first.
        window.timeDrives(watching: ["/Users/me", "/Users/me/notes"])
        XCTAssertEqual(window.drivesTold, ["/Volumes/Photos": 1, "/Volumes/Music": 2])
        window.timeDrives(watching: ["/Users/me", "/Volumes/Photos"])
        XCTAssertEqual(window.drivesTold, ["/Volumes/Music": 2], "the other drive still being read")
        window.timeDrives(watching: ["/Users/me", "/Volumes/Music", "/Volumes/Photos"])
        XCTAssertEqual(window.drivesTold, [:])
    }
}

/// Mounts a real disk image where Finder shows it, so only when asked for:
/// ARGUS_DRIVE_IMAGE=~/tmp/argus-test/drive.dmg swift test --filter DriveImageTests
/// The image holds trips/zanzibar-sunset-unique.jpg and is made writable, as macOS's own
/// read-only images are left out.
@MainActor
final class DriveImageTests: XCTestCase {
    func testADriveIsFoundWhileMountedAndLetGoOfOnEject() async throws {
        guard let image = ProcessInfo.processInfo.environment["ARGUS_DRIVE_IMAGE"] else {
            throw XCTSkip("mounts a disk image; set ARGUS_DRIVE_IMAGE to run it")
        }
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        var lists: [UInt64] = []
        var ready: UInt64 = 0
        var told: TimeInterval = 0
        let hatch = try XCTUnwrap(
            Hatch(root: root.path, fullDisk: true) { event in
                switch event {
                case .indexed(let count): lists.append(count)
                case .rebuilt(let count, let took):
                    lists.append(count)
                    print("list in use \(ms(since: told)) ms after macOS told, built in \(took) ms")
                case .ready(let ticket): ready = ticket
                default: break
                }
            })
        try await until { !lists.isEmpty }
        hatch.watch()
        let home = lists[0]
        var volume = ""
        // As the app does: macOS's question lets go first, then the window re-watches.
        let drives = Drives(
            mounted: [], letGo: { _ = hatch.letGo($0) },
            added: { path in
                told = ProcessInfo.processInfo.systemUptime
                guard hatch.volumeAdded(path) else { return }
                volume = path
            },
            removing: { path in
                told = ProcessInfo.processInfo.systemUptime
                hatch.volumeRemoved(path)
                print("let go \(String(format: "%.2f", (ProcessInfo.processInfo.systemUptime - told) * 1000)) ms after macOS told")
                XCTAssertFalse(hatch.watching.contains(path), "let go of before it unmounts")
            })

        // Lists are built at least 0.5 s apart; a drive plugged in long after the last one waits for none.
        try await Task.sleep(for: .seconds(1))
        try await hdiutil("attach", image)
        try await until { lists.count > 1 }
        hatch.watch()
        print("\(lists.last! - home) entries on the drive")
        XCTAssertTrue(hatch.watching.contains(volume))
        let ticket = hatch.submit("zanzibar sunset")
        try await until { ready == ticket }
        let rows = try XCTUnwrap(hatch.take(ticket))
        XCTAssertEqual(rows.first?.folder, volume + "/trips")

        try await Task.sleep(for: .seconds(1))
        // As Finder ejects: no force, so anything still holding the drive fails it.
        try await hdiutil("detach", volume)
        try await until { lists.count > 2 }
        XCTAssertEqual(lists, [home, lists[1], home], "no list between mount and eject")
        let after = hatch.submit("zanzibar sunset")
        try await until { ready == after }
        XCTAssertEqual(hatch.take(after), [])
        _ = drives
    }

    /// Wants an image with tens of thousands of folders, so its scan is still under way.
    func testAnEjectWhileTheMainThreadIsBusySucceedsFirstTime() async throws {
        guard let image = ProcessInfo.processInfo.environment["ARGUS_DRIVE_IMAGE"] else {
            throw XCTSkip("mounts a disk image; set ARGUS_DRIVE_IMAGE to run it")
        }
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        var lists = 0
        let hatch = try XCTUnwrap(Hatch(root: root.path, fullDisk: true) { _ in lists += 1 })
        try await until { lists == 1 }
        var volume = ""
        let drives = Drives(
            mounted: [], letGo: { _ = hatch.letGo($0) },
            added: { path in if hatch.volumeAdded(path) { volume = path } },
            removing: { hatch.volumeRemoved($0) })
        try await hdiutil("attach", image)
        try await until { !volume.isEmpty && hatch.health().busy }
        // Into the read itself: on 40,000 folders, about 0.35 s of waiting between lists, then 0.7 s.
        try await Task.sleep(for: .milliseconds(600))
        XCTAssertEqual(lists, 1, "still reading the drive")
        // The main thread waits here throughout, so only macOS's own question reaches the core.
        let status = DispatchQueue.global().sync {
            let detach = Process()
            detach.executableURL = URL(fileURLWithPath: "/usr/bin/hdiutil")
            detach.arguments = ["detach", volume]
            try? detach.run()
            detach.waitUntilExit()
            return detach.terminationStatus
        }
        if status != 0 { try await hdiutil("detach", "-force", volume) }
        XCTAssertEqual(status, 0, "the drive was in use")
        _ = drives
    }

    func testADriveMountedOutOfFindersSightIsLeftOut() async throws {
        guard let image = ProcessInfo.processInfo.environment["ARGUS_DRIVE_IMAGE"] else {
            throw XCTSkip("mounts a disk image; set ARGUS_DRIVE_IMAGE to run it")
        }
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        var lists = 0
        let hatch = try XCTUnwrap(Hatch(root: root.path, fullDisk: true) { _ in lists += 1 })
        try await until { lists == 1 }
        var heard: [String] = []
        let drives = Drives(
            mounted: [],
            added: { heard.append($0); XCTAssertFalse(hatch.volumeAdded($0), "hidden from Finder") },
            removing: { XCTAssertFalse(hatch.volumeRemoved($0)) })
        try await hdiutil("attach", "-nobrowse", image)
        try await until { !heard.isEmpty }
        try await hdiutil("detach", heard[0])
        try await Task.sleep(for: .seconds(1))
        XCTAssertEqual(lists, 1, "no list for it")
        _ = drives
    }

    /// Another app takes 12 s to answer, longer than macOS waits, so the drive unmounts after
    /// 10.7 s. Listing it again before then would read it while the unmount starts.
    func testAnEjectWaitingOnASlowAppIsNeverListedAgain() async throws {
        let (hatch, drives, _) = try await mountedDrive()
        let slow = SlowApp(volume: drives.volume, answers: [(12, false)])
        defer { slow.stop() }
        let asked = ProcessInfo.processInfo.systemUptime
        let status = try await hdiutilStatus("detach", drives.volume)
        print("unmounted after \(ms(since: asked)) ms")
        try await Task.sleep(for: .seconds(6))
        if status != 0 { try await hdiutil("detach", "-force", drives.volume) }
        XCTAssertEqual(status, 0, "the drive was in use")
        XCTAssertEqual(drives.heard, ["+" + drives.volume, "-" + drives.volume], "never listed again")
        XCTAssertEqual(drives.drives?.leaving, [])
        _ = hatch
    }

    /// The first eject is refused, and the second waits 4.5 s on an app: past the first one's
    /// check, had it not started over.
    func testAnEjectTriedAgainAfterARefusedOneIsNeverListedAgain() async throws {
        let (hatch, drives, _) = try await mountedDrive()
        let slow = SlowApp(volume: drives.volume, answers: [(5, true), (4.5, false)])
        defer { slow.stop() }
        let refused = try await hdiutilStatus("detach", drives.volume)
        XCTAssertNotEqual(refused, 0, "the app said no")
        try await Task.sleep(for: .seconds(2))
        let asked = ProcessInfo.processInfo.systemUptime
        let status = try await hdiutilStatus("detach", drives.volume)
        print("unmounted \(ms(since: asked)) ms after the second eject")
        try await Task.sleep(for: .seconds(16))
        if status != 0 { try await hdiutil("detach", "-force", drives.volume) }
        XCTAssertEqual(status, 0, "the drive was in use")
        XCTAssertEqual(drives.heard, ["+" + drives.volume, "-" + drives.volume], "never listed again")
        _ = hatch
    }

    func testADriveAnAppWouldNotLetGoIsListedAgain() async throws {
        let (hatch, drives, lists) = try await mountedDrive()
        let slow = SlowApp(volume: drives.volume, answers: [(3, true)])
        defer { slow.stop() }
        let asked = ProcessInfo.processInfo.systemUptime
        let refused = try await hdiutilStatus("detach", drives.volume)
        XCTAssertNotEqual(refused, 0, "the app said no")
        try await until(seconds: 20) { drives.heard.count == 3 }
        print("listed again \(ms(since: asked)) ms after the eject")
        XCTAssertEqual(drives.heard, ["+" + drives.volume, "-" + drives.volume, "+" + drives.volume])
        // Let go of, then read again.
        try await until { lists.count == 4 }
        try await hdiutil("detach", drives.volume)
        _ = hatch
    }

    /// A core and the drives it hears of, as the app wires them, with the image mounted and read.
    private func mountedDrive() async throws -> (Hatch, HeardDrives, Counter) {
        guard let image = ProcessInfo.processInfo.environment["ARGUS_DRIVE_IMAGE"] else {
            throw XCTSkip("mounts a disk image; set ARGUS_DRIVE_IMAGE to run it")
        }
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        let lists = Counter()
        let hatch = try XCTUnwrap(Hatch(root: root.path, fullDisk: true) { _ in lists.count += 1 })
        try await until { lists.count == 1 }
        let heard = HeardDrives()
        heard.drives = Drives(
            mounted: [], letGo: { _ = hatch.letGo($0) },
            added: { path in
                guard hatch.volumeAdded(path) else { return }
                heard.volume = path
                heard.heard.append("+" + path)
            },
            removing: { path in
                guard path == heard.volume else { return }
                hatch.volumeRemoved(path)
                heard.heard.append("-" + path)
            })
        try await hdiutil("attach", image)
        try await until { lists.count == 2 }
        return (hatch, heard, lists)
    }

    /// Waits without holding the main thread, where macOS's mount notes arrive.
    private func hdiutil(_ arguments: String...) async throws {
        let status = try await hdiutilStatus(arguments)
        XCTAssertEqual(status, 0, "hdiutil \(arguments.joined(separator: " "))")
    }

    private func hdiutilStatus(_ arguments: String...) async throws -> Int32 {
        try await hdiutilStatus(arguments)
    }

    private func hdiutilStatus(_ arguments: [String]) async throws -> Int32 {
        try await withCheckedThrowingContinuation { (done: CheckedContinuation<Int32, any Error>) in
            do {
                _ = try Process.run(URL(fileURLWithPath: "/usr/bin/hdiutil"), arguments: arguments) {
                    done.resume(returning: $0.terminationStatus)
                }
            } catch {
                done.resume(throwing: error)
            }
        }
    }

    private func until(seconds: TimeInterval = 10, _ done: () -> Bool) async throws {
        let deadline = ProcessInfo.processInfo.systemUptime + seconds
        while !done() {
            guard ProcessInfo.processInfo.systemUptime < deadline else { throw CancellationError() }
            try await Task.sleep(for: .milliseconds(2))
        }
    }

}

@MainActor
private final class Counter {
    var count = 0
}

@MainActor
private final class HeardDrives {
    var drives: Drives?
    var volume = ""
    var heard: [String] = []
}

/// Another app that answers macOS's question before each unmount of `volume`, slowly: each
/// answer in turn takes that many seconds and may refuse. Once they run out it says yes at once.
private final class SlowApp: @unchecked Sendable {
    private let volume: String
    private var answers: [(TimeInterval, Bool)]
    private let session: DASession
    private let queue = DispatchQueue(label: "slow app")

    init(volume: String, answers: [(TimeInterval, Bool)]) {
        self.volume = volume
        self.answers = answers
        session = DASessionCreate(kCFAllocatorDefault)!
        DARegisterDiskUnmountApprovalCallback(
            session, nil,
            { disk, context in
                let app = Unmanaged<SlowApp>.fromOpaque(context!).takeUnretainedValue()
                let description = DADiskCopyDescription(disk) as? [CFString: Any]
                let path = (description?[kDADiskDescriptionVolumePathKey] as? URL)?.path
                return path == app.volume ? app.answer() : nil
            }, Unmanaged.passUnretained(self).toOpaque())
        DASessionSetDispatchQueue(session, queue)
    }

    private func answer() -> Unmanaged<DADissenter>? {
        guard !answers.isEmpty else { return nil }
        let (seconds, refuses) = answers.removeFirst()
        Thread.sleep(forTimeInterval: seconds)
        guard refuses else { return nil }
        return Unmanaged.passRetained(DADissenterCreate(kCFAllocatorDefault, DAReturn(kDAReturnBusy), nil))
    }

    func stop() {
        DASessionSetDispatchQueue(session, nil)
        queue.sync {}
    }
}

private func ms(since start: TimeInterval) -> Int {
    Int((ProcessInfo.processInfo.systemUptime - start) * 1000)
}

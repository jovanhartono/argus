import AppKit
import XCTest

@testable import Telescope

@MainActor
final class DrivesTests: XCTestCase {
    private let center = NotificationCenter()
    private var heard: [String] = []

    private func drives(mounted: [String] = []) -> Drives {
        Drives(
            center: center, mounted: mounted, added: { [unowned self] in heard.append("+" + $0) },
            removing: { [unowned self] in heard.append("-" + $0) })
    }

    private func post(_ name: Notification.Name, _ path: String?) {
        let info = path.map { [NSWorkspace.volumeURLUserInfoKey: URL(fileURLWithPath: $0, isDirectory: true)] }
        center.post(name: name, object: NSWorkspace.shared, userInfo: info)
    }

    func testVolumesMountedAtLaunchArePassedOnFirstInOrder() {
        let drives = drives(mounted: ["/", "/Volumes/Photos", "/Volumes/Backup"])
        XCTAssertEqual(heard, ["+/", "+/Volumes/Photos", "+/Volumes/Backup"])
        _ = drives
    }

    func testAPluggedInDriveIsAddedAndAnEjectedOneLetGoBeforeItUnmounts() {
        let drives = drives()
        post(NSWorkspace.didMountNotification, "/Volumes/Photos")
        post(NSWorkspace.willUnmountNotification, "/Volumes/Photos")
        post(NSWorkspace.didUnmountNotification, "/Volumes/Photos")
        XCTAssertEqual(heard, ["+/Volumes/Photos", "-/Volumes/Photos"], "nothing left to do once it is gone")
        _ = drives
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
        XCTAssertEqual(ResultCell.place("/Volumes/Photos/trips"), "Photos · trips")
        XCTAssertEqual(ResultCell.place("/Volumes/My Drive/trips/2024 summer"), "My Drive · trips/2024 summer")
        XCTAssertEqual(ResultCell.place("/Users/me/notes"), "/Users/me/notes")
        XCTAssertEqual(ResultCell.place("~/notes"), "~/notes")
        XCTAssertEqual(ResultCell.place("/Volumes"), "/Volumes")
        XCTAssertEqual(ResultCell.place("/Volumes/"), "/Volumes/")
        XCTAssertEqual(ResultCell.place("/VolumesX/a"), "/VolumesX/a")
        XCTAssertEqual(ResultCell.place(""), "")
    }

    func testARowOnADriveShowsTheDrivesName() {
        let cell = ResultCell()
        cell.show(Row(name: "beach.jpg", folder: "/Volumes/Photos/trips", isFolder: false), icon: NSImage())
        XCTAssertEqual(cell.folder.stringValue, "Photos · trips")
        cell.show(
            TextHit(path: "/Volumes/Photos/notes/plan.txt", text: "plan", match: NSRange(location: 0, length: 4), line: 3),
            icon: NSImage())
        XCTAssertEqual(cell.folder.stringValue, "plan.txt, line 3 · Photos · notes")
    }

    func testTheWindowLeavesOutWhatTheCoreRefuses() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let window = SearchPanel(root: root.path, hasAccess: { true }, stage: OffStage())
        for path in ["/", "/Volumes/No Such Drive", root.path] {
            window.driveMounted(path)
            window.driveUnmounting(path)
        }
        XCTAssertNil(window.driveTold, "no list follows, so nothing waits to be timed")
    }
}

/// Mounts a real disk image where Finder shows it, so only when asked for:
/// TELESCOPE_DRIVE_IMAGE=~/tmp/telescope-test/drive.dmg swift test --filter DriveImageTests
/// The image holds trips/zanzibar-sunset-unique.jpg and is made writable, as macOS's own
/// read-only images are left out.
@MainActor
final class DriveImageTests: XCTestCase {
    func testADriveIsFoundWhileMountedAndLetGoOfOnEject() async throws {
        guard let image = ProcessInfo.processInfo.environment["TELESCOPE_DRIVE_IMAGE"] else {
            throw XCTSkip("mounts a disk image; set TELESCOPE_DRIVE_IMAGE to run it")
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
        let drives = Drives(
            mounted: [],
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

    func testADriveMountedOutOfFindersSightIsLeftOut() async throws {
        guard let image = ProcessInfo.processInfo.environment["TELESCOPE_DRIVE_IMAGE"] else {
            throw XCTSkip("mounts a disk image; set TELESCOPE_DRIVE_IMAGE to run it")
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

    /// Waits without holding the main thread, where macOS's mount notes arrive.
    private func hdiutil(_ arguments: String...) async throws {
        let status = try await withCheckedThrowingContinuation { (done: CheckedContinuation<Int32, any Error>) in
            do {
                _ = try Process.run(URL(fileURLWithPath: "/usr/bin/hdiutil"), arguments: arguments) {
                    done.resume(returning: $0.terminationStatus)
                }
            } catch {
                done.resume(throwing: error)
            }
        }
        XCTAssertEqual(status, 0, "hdiutil \(arguments.joined(separator: " "))")
    }

    private func until(_ done: () -> Bool) async throws {
        let deadline = ProcessInfo.processInfo.systemUptime + 10
        while !done() {
            guard ProcessInfo.processInfo.systemUptime < deadline else { throw CancellationError() }
            try await Task.sleep(for: .milliseconds(2))
        }
    }

}

private func ms(since start: TimeInterval) -> Int {
    Int((ProcessInfo.processInfo.systemUptime - start) * 1000)
}

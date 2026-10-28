import CoreServices
import Foundation

/// What the front Finder window shows, read only when the Finder hotkey is pressed: never at
/// launch, so macOS asks whether Telescope may control Finder the first time you use it.
enum FinderFolder: Equatable, Sendable {
    /// Its real path, as the list holds it.
    case folder(String)
    /// No Finder window is open, or the front one shows no folder, such as Recents.
    case none
    case denied
    /// Finder didn't answer in time, or answered with another error, by its code.
    case failed(Int)

    /// Waits for Finder, and the first time for your answer to macOS's question, so it is
    /// called off the main thread.
    nonisolated static func ask() -> FinderFolder {
        let finder = NSAppleEventDescriptor(bundleIdentifier: "com.apple.finder")
        let allowed = AEDeterminePermissionToAutomateTarget(
            finder.aeDesc, AEEventClass(kAECoreSuite), AEEventID(kAEGetData), true)
        guard allowed == noErr else { return outcome(Int(allowed)) }
        return read { try frontTarget(of: finder).sendEvent(options: .waitForReply, timeout: 2) }
    }

    /// "get target of Finder window 1 as alias", as an Apple event.
    nonisolated static func frontTarget(of finder: NSAppleEventDescriptor) -> NSAppleEventDescriptor {
        let window = specifier(
            want: code("brow"), form: DescType(formAbsolutePosition), data: NSAppleEventDescriptor(int32: 1),
            from: .null())
        let target = specifier(
            want: DescType(typeProperty), form: DescType(formPropertyID), data: NSAppleEventDescriptor(typeCode: code("fvtg")),
            from: window)
        let event = NSAppleEventDescriptor(
            eventClass: AEEventClass(kAECoreSuite), eventID: AEEventID(kAEGetData), targetDescriptor: finder,
            returnID: AEReturnID(kAutoGenerateReturnID), transactionID: AETransactionID(kAnyTransactionID))
        event.setParam(target, forKeyword: AEKeyword(keyDirectObject))
        event.setParam(NSAppleEventDescriptor(typeCode: DescType(typeAlias)), forKeyword: AEKeyword(keyAERequestedType))
        return event
    }

    /// Finder's reply, or the error sending threw.
    nonisolated static func read(_ answer: () throws -> NSAppleEventDescriptor) -> FinderFolder {
        let reply: NSAppleEventDescriptor
        do {
            reply = try answer()
        } catch {
            return outcome((error as NSError).code)
        }
        if let code = reply.paramDescriptor(forKeyword: AEKeyword(keyErrorNumber))?.int32Value, code != noErr {
            return outcome(Int(code))
        }
        guard let url = reply.paramDescriptor(forKeyword: AEKeyword(keyDirectObject))?.fileURLValue else {
            return .none
        }
        return folder(at: url)
    }

    /// Links and /var lead to the path the list holds, such as /private/var.
    nonisolated static func folder(at url: URL) -> FinderFolder {
        guard url.isFileURL, let real = realpath(url.path, nil) else { return .none }
        defer { free(real) }
        let path = String(cString: real)
        var isFolder: ObjCBool = false
        guard FileManager.default.fileExists(atPath: path, isDirectory: &isFolder), isFolder.boolValue else {
            return .none
        }
        return .folder(path)
    }

    nonisolated static func outcome(_ code: Int) -> FinderFolder {
        switch code {
        case Int(errAEEventNotPermitted): .denied
        // No window, a window on Recents or AirDrop, or Finder not running.
        case Int(errAENoSuchObject), Int(errAECoercionFail), Int(errAEIllegalIndex), Int(procNotFound): .none
        default: .failed(code)
        }
    }

    private nonisolated static func specifier(
        want: DescType, form: DescType, data: NSAppleEventDescriptor, from container: NSAppleEventDescriptor
    ) -> NSAppleEventDescriptor {
        let record = NSAppleEventDescriptor.record()
        record.setDescriptor(NSAppleEventDescriptor(typeCode: want), forKeyword: AEKeyword(keyAEDesiredClass))
        record.setDescriptor(NSAppleEventDescriptor(enumCode: form), forKeyword: AEKeyword(keyAEKeyForm))
        record.setDescriptor(data, forKeyword: AEKeyword(keyAEKeyData))
        record.setDescriptor(container, forKeyword: AEKeyword(keyAEContainer))
        return record.coerce(toDescriptorType: DescType(typeObjectSpecifier)) ?? record
    }

    /// Finder's four-letter codes, from its dictionary: `sdef /System/Library/CoreServices/Finder.app`.
    nonisolated static func code(_ text: StaticString) -> DescType {
        text.withUTF8Buffer { $0.reduce(0) { $0 << 8 | DescType($1) } }
    }
}

extension Row {
    /// A folder to search inside, named as Tab into it would name it.
    init(folderAt path: String) {
        let url = URL(fileURLWithPath: path)
        let name = path == "/" ? "/" : url.lastPathComponent
        self.init(name: name, folder: path == "/" ? "" : url.deletingLastPathComponent().path, isFolder: true)
    }
}

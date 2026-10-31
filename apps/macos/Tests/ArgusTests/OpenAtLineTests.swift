import XCTest

@testable import Argus

final class OpenAtLineTests: XCTestCase {
    private func link(_ app: String, _ path: String = "/Users/me/notes/plan.rs", line: UInt64 = 12) -> String? {
        switch LineLink.link(app: app, path: path, line: line) {
        case .url(let url): url.absoluteString
        case .xed(let arguments): "xed " + arguments.joined(separator: " ")
        case nil: nil
        }
    }

    func testEachCodeEditorGetsItsOwnLink() {
        XCTAssertEqual(link("com.microsoft.VSCode"), "vscode://file/Users/me/notes/plan.rs:12")
        XCTAssertEqual(link("com.microsoft.VSCodeInsiders"), "vscode-insiders://file/Users/me/notes/plan.rs:12")
        XCTAssertEqual(link("com.todesktop.230313mzl4w4u92"), "cursor://file/Users/me/notes/plan.rs:12")
        XCTAssertEqual(link("dev.zed.Zed"), "zed://file/Users/me/notes/plan.rs:12")
        XCTAssertEqual(link("dev.zed.Zed-Preview"), "zed://file/Users/me/notes/plan.rs:12")
        XCTAssertEqual(link("com.sublimetext.4"), "subl://open?url=file:///Users/me/notes/plan.rs&line=12")
        XCTAssertEqual(link("com.apple.dt.Xcode"), "xed --line 12 /Users/me/notes/plan.rs")
    }

    func testTextEditAndOtherAppsOpenTheFileAtItsTop() {
        XCTAssertNil(link("com.apple.TextEdit"))
        XCTAssertNil(link("com.apple.Preview"))
        XCTAssertNil(link(""))
    }

    /// A name with spaces, letters outside English, "#" or "&" still reaches the right file.
    func testOddNamesSurviveTheTrip() throws {
        let path = "/Users/me/Notes #2 & más/日本 plan.txt"
        let code = try XCTUnwrap(LineLink.link(app: "com.microsoft.VSCode", path: path, line: 3))
        guard case .url(let url) = code else { return XCTFail("\(code)") }
        XCTAssertEqual(url.scheme, "vscode")
        XCTAssertEqual(url.host, "file")
        XCTAssertNil(url.query, "a # or ? in the name ends nothing early")
        XCTAssertNil(url.fragment)
        XCTAssertEqual(url.path, path + ":3")

        let sublime = try XCTUnwrap(LineLink.link(app: "com.sublimetext.4", path: path, line: 3))
        guard case .url(let link) = sublime else { return XCTFail("\(sublime)") }
        let items = try XCTUnwrap(URLComponents(url: link, resolvingAgainstBaseURL: false)?.queryItems)
        XCTAssertEqual(items.map(\.name), ["url", "line"])
        XCTAssertEqual(items.last?.value, "3")
        let file = try XCTUnwrap(items.first?.value.flatMap(URL.init(string:)))
        XCTAssertEqual(file.path, path)

        XCTAssertEqual(
            LineLink.link(app: "com.apple.dt.Xcode", path: path, line: 3), .xed(["--line", "3", path]),
            "handed to xed as is, with no shell to split it")
    }
}

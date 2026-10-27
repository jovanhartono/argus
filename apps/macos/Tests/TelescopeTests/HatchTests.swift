import XCTest

@testable import Telescope

@MainActor
final class HatchTests: XCTestCase {
    func testASearchComesBackAsRows() async throws {
        let indexed = expectation(description: "indexed")
        let ready = expectation(description: "ready")
        var answered: UInt64 = 0
        let hatch = try XCTUnwrap(
            Hatch(standIn: 10_000) { event in
                switch event {
                case .indexed: indexed.fulfill()
                case .ready(let ticket):
                    answered = ticket
                    ready.fulfill()
                case .failed: XCTFail("the core stopped")
                }
            })
        await fulfillment(of: [indexed], timeout: 5)

        let ticket = hatch.submit("readme")
        await fulfillment(of: [ready], timeout: 5)

        XCTAssertEqual(answered, ticket)
        let rows = try XCTUnwrap(hatch.take(ticket))
        XCTAssertFalse(rows.isEmpty)
        XCTAssertTrue(rows.allSatisfy { $0.name.lowercased().contains("readme") })
        XCTAssertNil(hatch.take(ticket), "rows are handed over once")
    }
}

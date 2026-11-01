// Export macOS Calendar events as JSON for the aw-timer dashboard.
// Usage: swift scripts/export-calendar.swift [days] > calendar.json
// The first run asks for Calendar access for the app that runs it (e.g. Terminal).
import EventKit
import Foundation
let store = EKEventStore()
let sem = DispatchSemaphore(value: 0)
var granted = false
if #available(macOS 14.0, *) {
    store.requestFullAccessToEvents { ok, err in granted = ok; if let e = err { FileHandle.standardError.write("\(e)\n".data(using: .utf8)!) }; sem.signal() }
} else {
    store.requestAccess(to: .event) { ok, _ in granted = ok; sem.signal() }
}
sem.wait()
guard granted else { FileHandle.standardError.write("no calendar access\n".data(using: .utf8)!); exit(2) }
let days = Int(CommandLine.arguments.dropFirst().first ?? "35") ?? 35
let cal = Calendar.current
var startComp = cal.dateComponents([.year, .month, .day], from: Date())
startComp.hour = 4
let todayFour = cal.date(from: startComp)!
let start = cal.date(byAdding: .day, value: -days, to: todayFour)!
let end = cal.date(byAdding: .day, value: 2, to: todayFour)!
let pred = store.predicateForEvents(withStart: start, end: end, calendars: nil)
let iso = ISO8601DateFormatter()
var out: [[String: Any]] = []
for e in store.events(matching: pred) {
    out.append(["title": e.title ?? "", "start": iso.string(from: e.startDate), "end": iso.string(from: e.endDate), "all_day": e.isAllDay, "calendar": e.calendar.title])
}
let data = try! JSONSerialization.data(withJSONObject: out)
print(String(data: data, encoding: .utf8)!)

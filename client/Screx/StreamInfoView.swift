import Combine
import SwiftUI

/// Per-second session diagnostics, separate from `StreamViewModel` so that their updates
/// re-render only the views that display them.
@MainActor
final class StreamStats: ObservableObject {
    @Published private(set) var receiveRateText = "0 B/s"
    @Published private(set) var sendRateText = "0 B/s"
    /// Control-channel round-trip time from the in-session `PING` probe.
    @Published private(set) var latencyText = "—"
    /// Inter-sample variation of the control-channel RTT.
    @Published private(set) var jitterText = "—"

    nonisolated init() {}

    func setRates(receive: String, send: String) {
        if receive != receiveRateText { receiveRateText = receive }
        if send != sendRateText { sendRateText = send }
    }

    func setLatency(rtt: String, jitter: String) {
        if rtt != latencyText { latencyText = rtt }
        if jitter != jitterText { jitterText = jitter }
    }

    func reset() {
        setRates(receive: "0 B/s", send: "0 B/s")
        setLatency(rtt: "—", jitter: "—")
    }
}

/// Small connection card: hostname, transfer rates, latency, and a plain Disconnect action.
struct StreamInfoView: View {
    @ObservedObject var model: StreamViewModel

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(model.sessionDisplayName.isEmpty ? "Screx" : model.sessionDisplayName)
                .font(.subheadline.weight(.semibold))
                .lineLimit(1)
                .truncationMode(.middle)

            StreamStatsRows(stats: model.stats)

            Button("Disconnect") { model.disconnect() }
                .font(.caption.weight(.medium))
                .buttonStyle(.plain)
                .foregroundStyle(.tint)
                .padding(.top, 2)
        }
        .font(.caption)
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: 300, alignment: .leading)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
    }
}

private struct StreamStatsRows: View {
    @ObservedObject var stats: StreamStats

    var body: some View {
        HStack(spacing: 12) {
            rate(icon: "arrow.down", value: stats.receiveRateText, name: "Receiving")
            rate(icon: "arrow.up", value: stats.sendRateText, name: "Sending")
        }
        HStack(spacing: 12) {
            stat("RTT", stats.latencyText)
            stat("Jitter", stats.jitterText)
        }
    }

    private func rate(icon: String, value: String, name: String) -> some View {
        HStack(spacing: 3) {
            Image(systemName: icon)
                .imageScale(.small)
                .foregroundStyle(.secondary)
            Text(value).monospacedDigit()
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(name)
        .accessibilityValue(value)
    }

    private func stat(_ label: String, _ value: String) -> some View {
        HStack(spacing: 3) {
            Text(label).foregroundStyle(.secondary)
            Text(value).monospacedDigit()
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(label)
        .accessibilityValue(value)
    }
}

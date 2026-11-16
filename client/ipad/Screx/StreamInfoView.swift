import SwiftUI

/// Small connection card: hostname, transfer rates, latency, and a plain Disconnect action.
struct StreamInfoView: View {
    @ObservedObject var model: StreamViewModel

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(model.sessionDisplayName.isEmpty ? "Screx" : model.sessionDisplayName)
                .font(.subheadline.weight(.semibold))
                .lineLimit(1)
                .truncationMode(.middle)

            HStack(spacing: 12) {
                rate(icon: "arrow.down", value: model.receiveRateText, name: "Receiving")
                rate(icon: "arrow.up", value: model.sendRateText, name: "Sending")
            }

            HStack(spacing: 12) {
                stat("RTT", model.latencyText)
                stat("Jitter", model.jitterText)
            }

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

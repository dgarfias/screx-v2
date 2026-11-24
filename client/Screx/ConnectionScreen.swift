import Combine
import SwiftUI

/// Shown whenever no session is running. Saved hosts are the main content; typing an address
/// is the secondary path behind the + button.
struct ConnectionScreen: View {
    @ObservedObject var model: StreamViewModel
    @State private var showAddHost = false
    @State private var settingsHost: RecentConnection?
    @State private var confirmClear: ClearScope?

    private enum ClearScope: Identifiable {
        case unpinned, all
        var id: Self { self }
    }

    private var savedHosts: [RecentConnection] { model.pinnedConnections + model.unpinnedRecentConnections }

    /// A typed address isn't saved until its first successful connection. While it connects or
    /// after it fails, it gets a temporary tile so its progress and error have a place.
    private var pendingHost: RecentConnection? {
        guard let target = model.connectionTarget,
              !savedHosts.contains(where: { $0.id == target.id }),
              model.tileState(for: target.id) != .idle else { return nil }
        return target
    }

    private var tiles: [RecentConnection] { (pendingHost.map { [$0] } ?? []) + savedHosts }

    var body: some View {
        VStack(alignment: .leading, spacing: 28) {
            header
            if tiles.isEmpty {
                emptyState
            } else {
                ScrollView {
                    LazyVGrid(columns: [GridItem(.adaptive(minimum: 260, maximum: 400), spacing: 16)], spacing: 16) {
                        ForEach(tiles) { host in
                            HostTile(
                                host: host,
                                state: model.tileState(for: host.id),
                                isSaved: host.id != pendingHost?.id,
                                isOtherHostConnecting: model.isConnecting && host.id != model.connectionTarget?.id,
                                canPin: model.canPinMore,
                                showsSettingsAction: model.failureIsSettings,
                                onConnect: { model.connectRecent(host) },
                                onCancel: { model.disconnect() },
                                onTogglePin: { model.togglePinned(host) },
                                onRemove: { model.deleteConnection(host) },
                                onOpenSettings: { settingsHost = host }
                            )
                        }
                    }
                    .padding(.bottom, 32)
                }
            }
        }
        .padding(.horizontal, 32)
        .padding(.top, 28)
        .frame(maxWidth: 1240, maxHeight: .infinity, alignment: .top)
        .frame(maxWidth: .infinity)
        .background(Color(.systemGroupedBackground).ignoresSafeArea())
        .animation(.easeInOut(duration: 0.2), value: tiles.map(\.id))
        .sheet(item: $settingsHost) { host in
            HostSettingsSheet(
                mode: .edit(tileId: host.id, hostName: host.displayName),
                address: host.endpointLabel,
                settings: model.settings(for: host.id),
                limits: model.hostLimits[host.id],
                addressError: { model.addressError($0, for: host.id) },
                onSave: { model.updateHost(host.id, settings: $0, address: $1) }
            )
            // The model republishes about every second; comparing the seeded values lets SwiftUI
            // skip those re-renders, which would otherwise reset an open picker menu.
            .equatable()
        }
        .confirmationDialog(confirmClear == .all ? "Clear all hosts?" : "Clear unpinned hosts?",
                            isPresented: Binding(get: { confirmClear != nil }, set: { if !$0 { confirmClear = nil } }),
                            titleVisibility: .visible, presenting: confirmClear) { scope in
            Button("Clear", role: .destructive) {
                scope == .all ? model.clearAllHosts() : model.clearUnpinnedHosts()
            }
        } message: { _ in
            Text("You’ll need to pair again to reconnect.")
        }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Text("Screx")
                .font(.largeTitle.bold())
            Spacer()
            Button { showAddHost = true } label: {
                Image(systemName: "plus")
                    .font(.title2)
                    .frame(width: 44, height: 44)
            }
            .disabled(model.isConnecting)
            .accessibilityLabel("Add host")
            .sheet(isPresented: $showAddHost) {
                HostSettingsSheet(
                    mode: .add,
                    address: "",
                    settings: .defaults,
                    limits: nil,
                    addressError: { model.addressError($0, for: nil) },
                    onConnect: { model.connectNewHost(address: $1, settings: $0) }
                )
                .equatable()
            }
            if !model.recentConnections.isEmpty {
                Menu {
                    Button("Clear Unpinned Hosts", role: .destructive) { confirmClear = .unpinned }
                        .disabled(model.unpinnedRecentConnections.isEmpty)
                    Button("Clear All Hosts", role: .destructive) { confirmClear = .all }
                } label: {
                    Image(systemName: "ellipsis.circle")
                        .font(.title2)
                        .frame(width: 44, height: 44)
                }
                .disabled(model.isConnecting)
                .accessibilityLabel("More")
            }
        }
    }

    private var emptyState: some View {
        VStack(spacing: 20) {
            Image(systemName: "desktopcomputer")
                .font(.system(size: 56))
                .foregroundStyle(.secondary)
                .accessibilityHidden(true)
            Text("Tap + to connect to a server")
                .font(.title3.weight(.semibold))
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(.bottom, 80)
    }
}

private struct HostTile: View {
    let host: RecentConnection
    let state: HostTileState
    let isSaved: Bool
    let isOtherHostConnecting: Bool
    let canPin: Bool
    let showsSettingsAction: Bool
    let onConnect: () -> Void
    let onCancel: () -> Void
    let onTogglePin: () -> Void
    let onRemove: () -> Void
    let onOpenSettings: () -> Void

    private var isConnecting: Bool { if case .connecting = state { return true }; return false }
    private var failure: String? { if case let .failed(message) = state { return message }; return nil }
    private static let shape = RoundedRectangle(cornerRadius: 18, style: .continuous)

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top) {
                Image(systemName: "desktopcomputer")
                    .font(.title2)
                    .foregroundStyle(Color.accentColor)
                    .frame(width: 48, height: 48)
                    .background(Color.accentColor.opacity(0.15), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                    .allowsHitTesting(false)

                Spacer(minLength: 0)

                if !isConnecting {
                    Menu { menuItems } label: {
                        Image(systemName: "ellipsis.circle")
                            .font(.title3)
                            .foregroundStyle(.secondary)
                            .frame(width: 44, height: 44)
                    }
                    .disabled(isOtherHostConnecting)
                    .offset(x: 10, y: -10)
                    .accessibilityLabel("Options for \(host.displayName)")
                }
            }

            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(host.displayName)
                        .font(.title3.weight(.semibold))
                        .lineLimit(1)
                    if host.isPinned {
                        Image(systemName: "star.fill")
                            .font(.caption)
                            .foregroundStyle(.orange)
                            .accessibilityLabel("Pinned")
                    }
                }
                // Hosts without a name would show the same address twice.
                if host.displayName != host.endpointLabel {
                    Text(host.endpointLabel)
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            .allowsHitTesting(false)

            Spacer(minLength: 0)

            if isConnecting || failure != nil {
                HStack(alignment: .center, spacing: 10) {
                    statusLine
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .allowsHitTesting(false)

                    if isConnecting {
                        Button("Cancel", action: onCancel)
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                            .fixedSize()
                    } else if failure != nil && showsSettingsAction {
                        Button("Settings", action: onOpenSettings)
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                            .fixedSize()
                    }
                }
            }
        }
        .padding(18)
        .frame(maxWidth: .infinity, minHeight: 156, alignment: .topLeading)
        .background {
            Button(action: onConnect) {
                Self.shape.fill(Color(.secondarySystemGroupedBackground))
            }
            .buttonStyle(.plain)
            .disabled(isConnecting || isOtherHostConnecting)
            .accessibilityLabel("Connect to \(host.displayName)")
        }
        .clipShape(Self.shape)
        .overlay {
            Self.shape.strokeBorder(failure != nil ? Color.red.opacity(0.6) : Color.accentColor,
                                    lineWidth: failure != nil || isConnecting ? 1.5 : 0)
        }
        .opacity(isOtherHostConnecting ? 0.45 : 1)
        .contextMenu { if !isConnecting && !isOtherHostConnecting { menuItems } }
        .accessibilityElement(children: .contain)
    }

    @ViewBuilder
    private var statusLine: some View {
        if case let .connecting(label) = state {
            HStack(spacing: 8) {
                ProgressView()
                    .controlSize(.small)
                Text(label)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .minimumScaleFactor(0.85)
            }
            .frame(minHeight: 28, alignment: .leading)
        } else if let failure {
            Label {
                Text(failure)
                    .font(.subheadline)
                    .lineLimit(3)
                    .fixedSize(horizontal: false, vertical: true)
            } icon: {
                Image(systemName: "exclamationmark.triangle.fill")
                    .foregroundStyle(.red)
            }
            .frame(minHeight: 28, alignment: .leading)
        }
    }

    @ViewBuilder
    private var menuItems: some View {
        Button(action: onOpenSettings) {
            Label("Settings…", systemImage: "gearshape")
        }
        if isSaved {
            Button(action: onTogglePin) {
                Label(host.isPinned ? "Unpin" : "Pin", systemImage: host.isPinned ? "star.slash" : "star")
            }
            .disabled(!host.isPinned && !canPin)
            Button(role: .destructive, action: onRemove) {
                Label("Remove", systemImage: "trash")
            }
        }
    }
}

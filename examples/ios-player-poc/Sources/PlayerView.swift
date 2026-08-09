import AVKit
import SwiftUI

struct PlayerView: View {
    @StateObject private var model = PlayerModel()

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            VideoPlayer(player: model.player)
                .aspectRatio(16 / 9, contentMode: .fit)
                .background(.black)
                .accessibilityIdentifier("proxy-video-player")

            Text("Media Proxy Cache")
                .font(.title2.weight(.semibold))

            LabeledContent("Status", value: model.status)
                .accessibilityIdentifier("playback-status")
            LabeledContent("Proxy port", value: model.proxyPort == 0 ? "-" : String(model.proxyPort))
                .accessibilityIdentifier("proxy-port")
            LabeledContent("Position", value: model.elapsed)
                .accessibilityIdentifier("playback-position")
            LabeledContent("Cache", value: model.cacheSize)
                .accessibilityIdentifier("cache-size")

            HStack(spacing: 12) {
                Button(action: model.togglePlayback) {
                    Label("Play or pause", systemImage: "playpause.fill")
                }
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("toggle-playback")

                Button(action: model.seekForward) {
                    Label("Forward 10 seconds", systemImage: "goforward.10")
                }
                .buttonStyle(.bordered)
                .accessibilityIdentifier("seek-forward")
            }
        }
        .padding(20)
    }
}

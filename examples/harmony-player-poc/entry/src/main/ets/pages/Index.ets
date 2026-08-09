import { common } from '@kit.AbilityKit';
import { url } from '@kit.ArkTS';
import { media } from '@kit.MediaKit';
import { MediaProxyCache } from 'media-proxy-cache';

@Entry
@Component
struct Index {
  @State sourceUrl: string = '';
  @State status: string = 'Enter a Range-capable HTTP or HLS URL';
  @State playbackPosition: number = 0;
  @State duration: number = 0;
  private cache: MediaProxyCache | undefined = undefined;
  private player: media.AVPlayer | undefined = undefined;
  private surfaceController: XComponentController = new XComponentController();
  private surfaceId: string = '';
  private cacheDirectory: string = '';

  aboutToAppear(): void {
    const context = this.getUIContext().getHostContext() as common.UIAbilityContext;
    this.cacheDirectory = `${context.cacheDir}/media-proxy-cache`;
  }

  async aboutToDisappear(): Promise<void> {
    await this.releasePlayer();
    this.shutdownCache();
  }

  private async releasePlayer(): Promise<void> {
    const active = this.player;
    this.player = undefined;
    if (active !== undefined) {
      try {
        await active.release();
      } catch (reason) {
        this.status = (reason as Error).message;
      }
    }
  }

  private shutdownCache(): void {
    const active = this.cache;
    this.cache = undefined;
    if (active === undefined) {
      return;
    }
    try {
      active.stop();
    } catch (reason) {
      this.status = (reason as Error).message;
    }
    try {
      active.close();
    } catch (reason) {
      this.status = (reason as Error).message;
    }
  }

  private async startPlayback(): Promise<void> {
    if (this.sourceUrl.trim().length === 0) {
      this.status = 'Enter a Range-capable HTTP or HLS URL';
      return;
    }
    const origin = new url.URL(this.sourceUrl.trim());
    if (origin.hostname.length === 0) {
      this.status = 'Origin URL has no hostname';
      return;
    }
    try {
      await this.releasePlayer();
      this.shutdownCache();
      this.cache = MediaProxyCache.create(0, this.cacheDirectory, [origin.hostname]);
      const port = this.cache.start();
      const player = await media.createAVPlayer();
      this.player = player;
      if (this.surfaceId.length === 0) {
        this.status = 'Video surface is not ready';
        await this.releasePlayer();
        return;
      }
      player.surfaceId = this.surfaceId;
      player.on('stateChange', (state: string) => {
        this.status = state;
      });
      player.on('timeUpdate', (time: number) => {
        this.playbackPosition = time;
      });
      player.on('durationUpdate', (value: number) => {
        this.duration = value;
      });
      player.on('error', (error) => {
        this.status = `Error ${error.code}: ${error.message}`;
      });
      const source = media.createMediaSourceWithUrl(`http://127.0.0.1:${port}/media`, {
        'X-Original-Url': this.sourceUrl.trim(),
        'X-Cache-User-Id': 'harmony-poc-user',
        'X-Cache-Asset-Id': 'aa-video',
        'X-Cache-Asset-Revision': '1'
      });
      await player.setMediaSource(source);
      await player.prepare();
      await player.play();
    } catch (reason) {
      await this.releasePlayer();
      this.shutdownCache();
      this.status = (reason as Error).message;
    }
  }

  private play(): void {
    this.startPlayback().catch((reason: Error) => {
      this.status = reason.message;
    });
  }

  private pause(): void {
    this.player?.pause().catch((reason: Error) => {
      this.status = reason.message;
    });
  }

  private seekForward(): void {
    try {
      this.player?.seek(Math.min(this.playbackPosition + 10000, this.duration));
    } catch (reason) {
      this.status = (reason as Error).message;
    }
  }

  private stopPlayback(): void {
    this.releasePlayer().catch((reason: Error) => {
      this.status = reason.message;
    });
  }

  build() {
    Column({ space: 16 }) {
      Text($r('app.string.app_name'))
        .fontSize(28)
        .fontWeight(FontWeight.Bold)
        .width('100%')
      TextInput({ text: this.sourceUrl, placeholder: $r('app.string.source_label') })
        .onChange((value: string) => this.sourceUrl = value)
        .width('100%')
      Text(this.status).fontSize(16).width('100%')
      Text(`${Math.floor(this.playbackPosition / 1000)}s / ${Math.floor(this.duration / 1000)}s`)
        .fontSize(14)
        .width('100%')
      XComponent({ type: XComponentType.SURFACE, controller: this.surfaceController })
        .onLoad(() => {
          this.surfaceId = this.surfaceController.getXComponentSurfaceId();
        })
        .width('100%')
        .aspectRatio(16 / 9)
        .backgroundColor(Color.Black)
      Row({ space: 12 }) {
        Button($r('app.string.play')).onClick(() => { this.play(); })
        Button($r('app.string.pause')).onClick(() => { this.pause(); })
        Button($r('app.string.seek')).onClick(() => { this.seekForward(); })
        Button($r('app.string.stop')).onClick(() => { this.stopPlayback(); })
      }.width('100%')
    }
    .width('100%')
    .height('100%')
    .padding(24)
    .justifyContent(FlexAlign.Center)
    .backgroundColor('#F5F7F8')
  }
}

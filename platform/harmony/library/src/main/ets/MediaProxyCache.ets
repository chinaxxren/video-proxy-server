import nativeModule from 'libproxy_server.so';

interface NativeMediaProxyCache {
  start(): number;
  stop(): void;
  close(): void;
  registerP2pDirectory(manifestJson: string, pieceDirectory: string): string;
  verifyP2pSource(sourceId: string): boolean;
  removeP2pSource(sourceId: string): boolean;
}

interface NativeFactory {
  create(port: number, cacheDirectory: string, allowedHosts: string[]): NativeMediaProxyCache;
}

interface NativeModule {
  MediaProxyCache: NativeFactory;
}

const core = nativeModule as NativeModule;

export class MediaProxyCache {
  private readonly core: NativeMediaProxyCache;
  private boundPort: number = 0;

  private constructor(coreCache: NativeMediaProxyCache) {
    this.core = coreCache;
  }

  static create(port: number, cacheDirectory: string, allowedHosts: string[]): MediaProxyCache {
    return new MediaProxyCache(core.MediaProxyCache.create(port, cacheDirectory, allowedHosts));
  }

  start(): number {
    this.boundPort = this.core.start();
    return this.boundPort;
  }

  stop(): void {
    this.core.stop();
    this.boundPort = 0;
  }

  close(): void {
    this.core.close();
    this.boundPort = 0;
  }

  registerP2PDirectory(manifestJson: string, pieceDirectory: string): string {
    if (manifestJson.trim().length === 0 || !pieceDirectory.startsWith('/')) {
      throw new Error('P2P manifest and absolute piece directory are required');
    }
    return this.core.registerP2pDirectory(manifestJson, pieceDirectory);
  }

  verifyP2PSource(sourceId: string): boolean {
    return this.core.verifyP2pSource(sourceId);
  }

  removeP2PSource(sourceId: string): boolean {
    return this.core.removeP2pSource(sourceId);
  }

  p2pPlaybackUrl(sourceId: string): string {
    if (this.boundPort === 0) {
      throw new Error('MediaProxyCache is not running');
    }
    if (!/^[1-9][0-9]*$/.test(sourceId)) {
      throw new Error('Invalid P2P source ID');
    }
    return `http://127.0.0.1:${this.boundPort}/p2p/${sourceId}`;
  }
}

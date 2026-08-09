import nativeModule from 'libproxy_server.so';

interface NativeMediaProxyCache {
  start(): number;
  stop(): void;
  close(): void;
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

  private constructor(coreCache: NativeMediaProxyCache) {
    this.core = coreCache;
  }

  static create(port: number, cacheDirectory: string, allowedHosts: string[]): MediaProxyCache {
    return new MediaProxyCache(core.MediaProxyCache.create(port, cacheDirectory, allowedHosts));
  }

  start(): number {
    return this.core.start();
  }

  stop(): void {
    this.core.stop();
  }

  close(): void {
    this.core.close();
  }
}

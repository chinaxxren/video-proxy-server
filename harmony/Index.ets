export interface MediaProxyCacheConfiguration {
  cacheDirectory: string
  allowedHosts: string[]
  port?: number
}

export class MediaProxyCache {
  private boundPort: number = 0
  private constructor(private readonly nativeHandle: bigint) {}

  static create(configuration: MediaProxyCacheConfiguration): MediaProxyCache {
    const port = configuration.port ?? 0
    if (port < 0 || port > 65535 || !configuration.cacheDirectory || configuration.allowedHosts.length === 0) {
      throw new Error('invalid MediaProxyCache configuration')
    }
    const handle = nativeCreate(port, configuration.cacheDirectory, configuration.allowedHosts.join(','))
    if (handle === 0n) throw new Error('native MediaProxyCache creation failed')
    return new MediaProxyCache(handle)
  }

  start(): number {
    const port = nativeStart(this.nativeHandle)
    if (port === 0) throw new Error('proxy server failed to start')
    this.boundPort = port
    return port
  }

  addAuthorizedTorrent(magnetUri: string): string {
    if (this.boundPort === 0 || !magnetUri.startsWith('magnet:?')) throw new Error('invalid torrent request')
    return nativeAddAuthorizedTorrent(this.nativeHandle, magnetUri)
  }

  removeTorrent(torrentId: string, deleteFiles: boolean = false): boolean {
    if (!/^\d+$/.test(torrentId)) throw new Error('invalid torrent ID')
    return nativeRemoveTorrent(this.nativeHandle, torrentId, deleteFiles)
  }

  torrentPlaybackUrl(torrentId: string, fileId: number): string {
    if (this.boundPort === 0 || !/^\d+$/.test(torrentId) || !Number.isInteger(fileId) || fileId < 0) {
      throw new Error('invalid torrent playback request')
    }
    return `http://127.0.0.1:${this.boundPort}/torrent/${torrentId}/${fileId}`
  }

  stop(): void { nativeStop(this.nativeHandle); this.boundPort = 0 }
  close(): void { nativeDestroy(this.nativeHandle); this.boundPort = 0 }
}

declare function nativeCreate(port: number, cacheDirectory: string, allowedHosts: string): bigint
declare function nativeStart(handle: bigint): number
declare function nativeStop(handle: bigint): void
declare function nativeDestroy(handle: bigint): void
declare function nativeAddAuthorizedTorrent(handle: bigint, magnetUri: string): string
declare function nativeRemoveTorrent(handle: bigint, torrentId: string, deleteFiles: boolean): boolean

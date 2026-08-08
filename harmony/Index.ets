export interface MediaProxyCacheConfiguration {
  cacheDirectory: string
  allowedHosts: string[]
  port?: number
}

export class MediaProxyCache {
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
    return port
  }

  stop(): void { nativeStop(this.nativeHandle) }
  close(): void { nativeDestroy(this.nativeHandle) }
}

declare function nativeCreate(port: number, cacheDirectory: string, allowedHosts: string): bigint
declare function nativeStart(handle: bigint): number
declare function nativeStop(handle: bigint): void
declare function nativeDestroy(handle: bigint): void

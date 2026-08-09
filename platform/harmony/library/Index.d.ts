export declare class MediaProxyCache {
  static create(port: number, cacheDirectory: string, allowedHosts: string[]): MediaProxyCache;
  start(): number;
  stop(): void;
  close(): void;
}

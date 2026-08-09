export declare class MediaProxyCache {
  static create(port: number, cacheDirectory: string, allowedHosts: string[]): MediaProxyCache;
  start(): number;
  stop(): void;
  close(): void;
  registerP2PDirectory(manifestJson: string, pieceDirectory: string): string;
  verifyP2PSource(sourceId: string): boolean;
  removeP2PSource(sourceId: string): boolean;
  p2pPlaybackUrl(sourceId: string): string;
}

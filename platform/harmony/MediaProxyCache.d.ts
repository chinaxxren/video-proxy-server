/** N-API adapter contract for the shared Rust C ABI. */
export interface TorrentFile { file_id: number; relative_path: string; length: number; }
export interface TorrentStatus {
  state: string; total_bytes: number; downloaded_bytes: number;
  uploaded_bytes: number; finished: boolean; error?: string | null;
}
export interface ProxyMetrics {
  requests: number; active_requests: number; request_errors: number;
  response_bytes: number; authorization_refreshes: number;
}
export declare class MediaProxyCache {
  static create(port: number, cacheDirectory: string, allowedHosts: string[]): MediaProxyCache;
  start(): number;
  stop(): void;
  metricsJson(): string;
  registerSource(identity: string, url: string): string;
  refreshSource(sourceId: string, url: string): boolean;
  removeSource(sourceId: string): boolean;
  setSourceRefreshProvider(provider: (sourceId: string) => string | null): boolean;
  playbackUrl(sourceId: string): string;
  close(): void;
  registerP2PDirectory(manifestJson: string, pieceDirectory: string): string;
  verifyP2PSource(sourceId: string): boolean;
  removeP2PSource(sourceId: string): boolean;
  p2pPlaybackUrl(sourceId: string): string;
  addAuthorizedTorrent(magnetUri: string): string;
  addAuthorizedTorrentFile(torrentBytes: Uint8Array): string;
  removeTorrent(torrentId: string, deleteFiles?: boolean): boolean;
  torrentPlaybackUrl(torrentId: string, fileId: number): string;
  torrentFiles(torrentId: string): TorrentFile[];
  selectTorrentFiles(torrentId: string, fileIds: number[]): boolean;
  torrentStatus(torrentId: string): TorrentStatus;
  pauseTorrent(torrentId: string): boolean;
  resumeTorrent(torrentId: string): boolean;
  setTorrentDownloadLimit(bytesPerSecond: number): boolean;
}

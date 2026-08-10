import nativeModule from 'libproxy_server.so';

interface NativeMediaProxyCache {
  start(): number;
  stop(): void;
  close(): void;
  registerP2pDirectory(manifestJson: string, pieceDirectory: string): string;
  verifyP2pSource(sourceId: string): boolean;
  removeP2pSource(sourceId: string): boolean;
  addAuthorizedTorrent(magnetUri: string): string;
  addAuthorizedTorrentFile(torrentBytes: Uint8Array): string;
  removeTorrent(torrentId: string, deleteFiles: boolean): boolean;
  torrentFilesJson(torrentId: string): string;
  selectTorrentFiles(torrentId: string, fileIds: number[]): boolean;
  torrentStatusJson(torrentId: string): string;
  setTorrentPaused(torrentId: string, paused: boolean): boolean;
  setTorrentDownloadLimit(bytesPerSecond: number): boolean;
}

export interface TorrentFile {
  file_id: number;
  relative_path: string;
  length: number;
}

export interface TorrentStatus {
  state: string;
  total_bytes: number;
  downloaded_bytes: number;
  uploaded_bytes: number;
  finished: boolean;
  error?: string | null;
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

  addAuthorizedTorrent(magnetUri: string): string {
    if (this.boundPort === 0 || !magnetUri.startsWith('magnet:?')) {
      throw new Error('A running proxy and valid Magnet URI are required');
    }
    return this.core.addAuthorizedTorrent(magnetUri);
  }

  addAuthorizedTorrentFile(torrentBytes: Uint8Array): string {
    if (this.boundPort === 0 || torrentBytes.byteLength === 0 || torrentBytes.byteLength > 4 * 1024 * 1024) {
      throw new Error('Invalid torrent metadata');
    }
    return this.core.addAuthorizedTorrentFile(torrentBytes);
  }

  removeTorrent(torrentId: string, deleteFiles: boolean = false): boolean {
    if (!/^[0-9]+$/.test(torrentId)) {
      throw new Error('Invalid torrent ID');
    }
    return this.core.removeTorrent(torrentId, deleteFiles);
  }

  torrentPlaybackUrl(torrentId: string, fileId: number): string {
    if (this.boundPort === 0 || !/^[0-9]+$/.test(torrentId) || !Number.isInteger(fileId) || fileId < 0) {
      throw new Error('Invalid torrent playback request');
    }
    return `http://127.0.0.1:${this.boundPort}/torrent/${torrentId}/${fileId}`;
  }

  torrentFiles(torrentId: string): TorrentFile[] {
    if (!/^[0-9]+$/.test(torrentId)) throw new Error('Invalid torrent ID');
    return JSON.parse(this.core.torrentFilesJson(torrentId)) as TorrentFile[];
  }

  selectTorrentFiles(torrentId: string, fileIds: number[]): boolean {
    if (!/^[0-9]+$/.test(torrentId) || fileIds.length === 0 || fileIds.length > 4096 ||
      fileIds.some((id: number) => !Number.isInteger(id) || id < 0 || id > 4294967295)) {
      throw new Error('Invalid torrent file selection');
    }
    return this.core.selectTorrentFiles(torrentId, fileIds);
  }

  torrentStatus(torrentId: string): TorrentStatus {
    if (!/^[0-9]+$/.test(torrentId)) throw new Error('Invalid torrent ID');
    return JSON.parse(this.core.torrentStatusJson(torrentId)) as TorrentStatus;
  }

  pauseTorrent(torrentId: string): boolean {
    if (!/^[0-9]+$/.test(torrentId)) throw new Error('Invalid torrent ID');
    return this.core.setTorrentPaused(torrentId, true);
  }

  resumeTorrent(torrentId: string): boolean {
    if (!/^[0-9]+$/.test(torrentId)) throw new Error('Invalid torrent ID');
    return this.core.setTorrentPaused(torrentId, false);
  }

  setTorrentDownloadLimit(bytesPerSecond: number): boolean {
    if (!Number.isInteger(bytesPerSecond) || bytesPerSecond < 0 || bytesPerSecond > 4294967295) {
      throw new Error('Invalid torrent download limit');
    }
    return this.core.setTorrentDownloadLimit(bytesPerSecond);
  }
}

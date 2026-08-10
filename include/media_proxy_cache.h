#ifndef MEDIA_PROXY_CACHE_H
#define MEDIA_PROXY_CACHE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct ProxyServerHandle ProxyServerHandle;
typedef size_t (*ProxySourceRefreshCallback)(
    void *context,
    uint64_t source_id,
    uint8_t *buffer,
    size_t capacity
);

ProxyServerHandle *proxy_server_create(uint16_t port, const char *cache_dir);
ProxyServerHandle *proxy_server_create_with_hosts(
    uint16_t port,
    const char *cache_dir,
    const char *allowed_hosts_csv
);
uint16_t proxy_server_start(ProxyServerHandle *handle);
void proxy_server_stop(ProxyServerHandle *handle);
void proxy_server_destroy(ProxyServerHandle *handle);
// Returns an opaque ID; identical identity+URL registrations reuse the same ID.
// Build the player URL as http://127.0.0.1:<port>/media/<id>.
// Never pass the signed URL directly to a player or include it in logs.
uint64_t proxy_source_register(ProxyServerHandle *handle, const char *identity, const char *url);
// Refreshes the signed URL for an existing ID without changing its cache identity.
uint8_t proxy_source_refresh(ProxyServerHandle *handle, uint64_t source_id, const char *url);
// Invalidates the ID. Existing playback requests may finish, future requests fail.
uint8_t proxy_source_remove(ProxyServerHandle *handle, uint64_t source_id);
// The callback may run on a Core worker thread. Write the refreshed URL as
// UTF-8 bytes (without a trailing NUL) and return its length. Return zero on
// failure. A length greater than capacity is rejected.
uint8_t proxy_source_set_refresh_callback(
    ProxyServerHandle *handle,
    ProxySourceRefreshCallback callback,
    void *context
);

#ifdef MEDIA_PROXY_CACHE_ENABLE_P2P
typedef size_t (*ProxyP2pPieceCallback)(
    void *context,
    size_t piece_index,
    uint8_t *buffer,
    size_t capacity
);
// The callback is called first with buffer=NULL/capacity=0 to query length,
// then again to fill the Host-owned bytes into Core's buffer.
// It must not call P2P Core functions or proxy_server_destroy reentrantly.
uint64_t proxy_p2p_source_register(
    ProxyServerHandle *handle,
    const uint8_t *manifest_json,
    size_t manifest_length,
    ProxyP2pPieceCallback callback,
    void *context
);
// Registers a managed-runtime-friendly provider that reads pieces from
// <piece_directory>/<piece_index>.piece. The Host must supply an absolute,
// app-private directory path and keep it available until the source is removed.
// Core still verifies every piece against the authorized manifest before serving it.
uint64_t proxy_p2p_source_register_directory(
    ProxyServerHandle *handle,
    const uint8_t *manifest_json,
    size_t manifest_length,
    const char *piece_directory
);
uint8_t proxy_p2p_source_verify_complete(ProxyServerHandle *handle, uint64_t source_id);
uint8_t proxy_p2p_source_remove(ProxyServerHandle *handle, uint64_t source_id);
#endif

#ifdef MEDIA_PROXY_CACHE_ENABLE_LIBRQBIT
// P2P network activity begins only after this explicitly authorized call.
// Returns a non-negative torrent ID, or -1 on failure.
int64_t proxy_torrent_add_authorized(
    ProxyServerHandle *handle,
    const char *magnet_uri,
    uint8_t explicitly_authorized
);
int64_t proxy_torrent_add_file_authorized(
    ProxyServerHandle *handle,
    const uint8_t *torrent_bytes,
    size_t length,
    uint8_t explicitly_authorized
);
// delete_files=0 forgets the session but preserves downloaded files.
uint8_t proxy_torrent_remove(
    ProxyServerHandle *handle,
    int64_t torrent_id,
    uint8_t delete_files
);
// JSON buffer APIs return required capacity including trailing NUL. Pass NULL
// to query capacity. They return 0 for an unknown ID or unavailable backend.
size_t proxy_torrent_files_json(
    ProxyServerHandle *handle,
    int64_t torrent_id,
    uint8_t *buffer,
    size_t capacity
);
size_t proxy_torrent_status_json(
    ProxyServerHandle *handle,
    int64_t torrent_id,
    uint8_t *buffer,
    size_t capacity
);
uint8_t proxy_torrent_set_paused(
    ProxyServerHandle *handle,
    int64_t torrent_id,
    uint8_t paused
);
// Session-wide bytes/second. Pass 0 to remove the download limit.
uint8_t proxy_torrent_set_download_limit(
    ProxyServerHandle *handle,
    uint32_t bytes_per_second
);
uint8_t proxy_torrent_select_files(
    ProxyServerHandle *handle,
    int64_t torrent_id,
    const uint32_t *file_ids,
    size_t file_count
);
#endif

#ifdef __cplusplus
}
#endif

#endif

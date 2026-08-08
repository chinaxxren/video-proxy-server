#ifndef MEDIA_PROXY_CACHE_H
#define MEDIA_PROXY_CACHE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct ProxyServerHandle ProxyServerHandle;

ProxyServerHandle *proxy_server_create(uint16_t port, const char *cache_dir);
ProxyServerHandle *proxy_server_create_with_hosts(
    uint16_t port,
    const char *cache_dir,
    const char *allowed_hosts_csv
);
uint16_t proxy_server_start(ProxyServerHandle *handle);
void proxy_server_stop(ProxyServerHandle *handle);
void proxy_server_destroy(ProxyServerHandle *handle);

#ifdef MEDIA_PROXY_CACHE_ENABLE_P2P
#include <stddef.h>
typedef size_t (*ProxyP2pPieceCallback)(
    void *context,
    size_t piece_index,
    uint8_t *buffer,
    size_t capacity
);
// The callback is called first with buffer=NULL/capacity=0 to query length,
// then again to fill the Host-owned bytes into Core's buffer.
// It must not call proxy_p2p_source_remove or proxy_server_destroy reentrantly.
uint64_t proxy_p2p_source_register(
    ProxyServerHandle *handle,
    const uint8_t *manifest_json,
    size_t manifest_length,
    ProxyP2pPieceCallback callback,
    void *context
);
uint8_t proxy_p2p_source_remove(ProxyServerHandle *handle, uint64_t source_id);
#endif

#ifdef __cplusplus
}
#endif

#endif

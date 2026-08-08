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
// Returns an opaque ID; identical identity+URL registrations reuse the same ID.
// Build the player URL as http://127.0.0.1:<port>/media/<id>.
// Never pass the signed URL directly to a player or include it in logs.
uint64_t proxy_source_register(ProxyServerHandle *handle, const char *identity, const char *url);
// Refreshes the signed URL for an existing ID without changing its cache identity.
uint8_t proxy_source_refresh(ProxyServerHandle *handle, uint64_t source_id, const char *url);
// Invalidates the ID. Existing playback requests may finish, future requests fail.
uint8_t proxy_source_remove(ProxyServerHandle *handle, uint64_t source_id);

#ifdef __cplusplus
}
#endif

#endif

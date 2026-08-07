#ifndef MEDIA_PROXY_CACHE_H
#define MEDIA_PROXY_CACHE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct ProxyServerHandle ProxyServerHandle;

ProxyServerHandle *proxy_server_create(uint16_t port, const char *cache_dir);
uint16_t proxy_server_start(ProxyServerHandle *handle);
void proxy_server_stop(ProxyServerHandle *handle);
void proxy_server_destroy(ProxyServerHandle *handle);

#ifdef __cplusplus
}
#endif

#endif

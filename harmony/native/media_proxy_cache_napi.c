#include <node_api.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include "media_proxy_cache.h"

typedef struct Entry { uint64_t id; ProxyServerHandle *handle; struct Entry *next; } Entry;
static Entry *entries;
static uint64_t next_id = 1;
static pthread_mutex_t entries_lock = PTHREAD_MUTEX_INITIALIZER;

static Entry *lookup(uint64_t id) {
    for (Entry *entry = entries; entry != NULL; entry = entry->next) if (entry->id == id) return entry;
    return NULL;
}

static napi_value result_u64(napi_env env, uint64_t value) {
    napi_value result; napi_create_bigint_uint64(env, value, &result); return result;
}

static napi_value undefined_value(napi_env env) {
    napi_value result; napi_get_undefined(env, &result); return result;
}

static napi_value create(napi_env env, napi_callback_info info) {
    size_t argc = 3; napi_value argv[3]; napi_get_cb_info(env, info, &argc, argv, NULL, NULL);
    if (argc != 3) return result_u64(env, 0);
    int64_t port; napi_get_value_int64(env, argv[0], &port);
    if (port < 0 || port > UINT16_MAX) return result_u64(env, 0);
    size_t cache_len = 0, hosts_len = 0;
    napi_get_value_string_utf8(env, argv[1], NULL, 0, &cache_len);
    napi_get_value_string_utf8(env, argv[2], NULL, 0, &hosts_len);
    char *cache = calloc(cache_len + 1, 1), *hosts = calloc(hosts_len + 1, 1);
    if (cache == NULL || hosts == NULL) { free(cache); free(hosts); return result_u64(env, 0); }
    napi_get_value_string_utf8(env, argv[1], cache, cache_len + 1, &cache_len);
    napi_get_value_string_utf8(env, argv[2], hosts, hosts_len + 1, &hosts_len);
    ProxyServerHandle *server = proxy_server_create_with_hosts((uint16_t)port, cache, hosts);
    free(cache); free(hosts);
    if (server == NULL) return result_u64(env, 0);
    Entry *entry = calloc(1, sizeof(*entry));
    if (entry == NULL) { proxy_server_destroy(server); return result_u64(env, 0); }
    pthread_mutex_lock(&entries_lock);
    entry->id = next_id++; entry->handle = server; entry->next = entries; entries = entry;
    pthread_mutex_unlock(&entries_lock);
    return result_u64(env, entry->id);
}

static napi_value start(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1]; napi_get_cb_info(env, info, &argc, argv, NULL, NULL);
    bool lossless; uint64_t id; napi_get_value_bigint_uint64(env, argv[0], &id, &lossless);
    pthread_mutex_lock(&entries_lock);
    Entry *entry = lookup(id);
    uint16_t port = entry == NULL ? 0 : proxy_server_start(entry->handle);
    pthread_mutex_unlock(&entries_lock);
    napi_value result; napi_create_uint32(env, port, &result); return result;
}

static napi_value stop(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1]; napi_get_cb_info(env, info, &argc, argv, NULL, NULL);
    bool lossless; uint64_t id; napi_get_value_bigint_uint64(env, argv[0], &id, &lossless);
    pthread_mutex_lock(&entries_lock); Entry *entry = lookup(id); if (entry) proxy_server_stop(entry->handle); pthread_mutex_unlock(&entries_lock); return undefined_value(env);
}

static napi_value destroy(napi_env env, napi_callback_info info) {
    size_t argc = 1; napi_value argv[1]; napi_get_cb_info(env, info, &argc, argv, NULL, NULL);
    bool lossless; uint64_t id; napi_get_value_bigint_uint64(env, argv[0], &id, &lossless);
    pthread_mutex_lock(&entries_lock);
    Entry **link = &entries; while (*link && (*link)->id != id) link = &(*link)->next; Entry *entry = *link;
    if (entry) { *link = entry->next; proxy_server_destroy(entry->handle); free(entry); }
    pthread_mutex_unlock(&entries_lock); return undefined_value(env);
}

static napi_value init(napi_env env, napi_value exports) {
    napi_property_descriptor props[] = {
        {"nativeCreate", NULL, create, NULL, NULL, NULL, napi_default, NULL},
        {"nativeStart", NULL, start, NULL, NULL, NULL, napi_default, NULL},
        {"nativeStop", NULL, stop, NULL, NULL, NULL, napi_default, NULL},
        {"nativeDestroy", NULL, destroy, NULL, NULL, NULL, napi_default, NULL},
    };
    napi_define_properties(env, exports, 4, props); return exports;
}
NAPI_MODULE(NODE_GYP_MODULE_NAME, init)

#include <node_api.h>
#include <stdint.h>
#include <stdio.h>
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

static napi_value result_bool(napi_env env, bool value) {
    napi_value result; napi_get_boolean(env, value, &result); return result;
}

static char *string_argument(napi_env env, napi_value value) {
    size_t length = 0;
    if (napi_get_value_string_utf8(env, value, NULL, 0, &length) != napi_ok) return NULL;
    char *result = calloc(length + 1, 1);
    if (result == NULL) return NULL;
    if (napi_get_value_string_utf8(env, value, result, length + 1, &length) != napi_ok) {
        free(result); return NULL;
    }
    return result;
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

#ifdef MEDIA_PROXY_CACHE_ENABLE_LIBRQBIT
static napi_value add_authorized_torrent(napi_env env, napi_callback_info info) {
    size_t argc = 2; napi_value argv[2]; napi_get_cb_info(env, info, &argc, argv, NULL, NULL);
    if (argc != 2) return NULL;
    bool lossless; uint64_t id; napi_get_value_bigint_uint64(env, argv[0], &id, &lossless);
    char *magnet = string_argument(env, argv[1]);
    if (magnet == NULL) return NULL;
    pthread_mutex_lock(&entries_lock);
    Entry *entry = lookup(id);
    int64_t torrent_id = entry == NULL ? -1 : proxy_torrent_add_authorized(entry->handle, magnet, 1);
    pthread_mutex_unlock(&entries_lock);
    free(magnet);
    if (torrent_id < 0) return NULL;
    char text[32]; snprintf(text, sizeof(text), "%lld", (long long)torrent_id);
    napi_value result; napi_create_string_utf8(env, text, NAPI_AUTO_LENGTH, &result); return result;
}

static napi_value remove_torrent(napi_env env, napi_callback_info info) {
    size_t argc = 3; napi_value argv[3]; napi_get_cb_info(env, info, &argc, argv, NULL, NULL);
    if (argc != 3) return result_bool(env, false);
    bool lossless, delete_files; uint64_t id;
    napi_get_value_bigint_uint64(env, argv[0], &id, &lossless);
    char *torrent_text = string_argument(env, argv[1]);
    napi_get_value_bool(env, argv[2], &delete_files);
    if (torrent_text == NULL) return result_bool(env, false);
    char *end = NULL; long long torrent_id = strtoll(torrent_text, &end, 10);
    bool valid = torrent_text[0] != '\0' && end != NULL && *end == '\0' && torrent_id >= 0;
    free(torrent_text);
    if (!valid) return result_bool(env, false);
    pthread_mutex_lock(&entries_lock);
    Entry *entry = lookup(id);
    bool removed = entry != NULL && proxy_torrent_remove(entry->handle, (int64_t)torrent_id, delete_files) == 1;
    pthread_mutex_unlock(&entries_lock);
    return result_bool(env, removed);
}

typedef size_t (*torrent_json_query)(ProxyServerHandle *, int64_t, uint8_t *, size_t);

static napi_value query_torrent_json(napi_env env, napi_callback_info info, torrent_json_query query) {
    size_t argc = 2; napi_value argv[2]; napi_get_cb_info(env, info, &argc, argv, NULL, NULL);
    if (argc != 2) return NULL;
    bool lossless; uint64_t id; napi_get_value_bigint_uint64(env, argv[0], &id, &lossless);
    char *torrent_text = string_argument(env, argv[1]);
    if (torrent_text == NULL) return NULL;
    char *end = NULL; long long torrent_id = strtoll(torrent_text, &end, 10);
    bool valid = torrent_text[0] != '\0' && end != NULL && *end == '\0' && torrent_id >= 0;
    free(torrent_text);
    if (!valid) return NULL;
    pthread_mutex_lock(&entries_lock);
    Entry *entry = lookup(id);
    size_t required = entry == NULL ? 0 : query(entry->handle, (int64_t)torrent_id, NULL, 0);
    if (required == 0 || required > 4 * 1024 * 1024 + 1) {
        pthread_mutex_unlock(&entries_lock); return NULL;
    }
    uint8_t *json = malloc(required);
    size_t written = json == NULL ? 0 : query(entry->handle, (int64_t)torrent_id, json, required);
    pthread_mutex_unlock(&entries_lock);
    if (written != required) { free(json); return NULL; }
    napi_value result; napi_create_string_utf8(env, (char *)json, required - 1, &result); free(json); return result;
}

static napi_value torrent_files_json(napi_env env, napi_callback_info info) {
    return query_torrent_json(env, info, proxy_torrent_files_json);
}

static napi_value torrent_status_json(napi_env env, napi_callback_info info) {
    return query_torrent_json(env, info, proxy_torrent_status_json);
}
#endif

static napi_value init(napi_env env, napi_value exports) {
    napi_property_descriptor props[] = {
        {"nativeCreate", NULL, create, NULL, NULL, NULL, napi_default, NULL},
        {"nativeStart", NULL, start, NULL, NULL, NULL, napi_default, NULL},
        {"nativeStop", NULL, stop, NULL, NULL, NULL, napi_default, NULL},
        {"nativeDestroy", NULL, destroy, NULL, NULL, NULL, napi_default, NULL},
#ifdef MEDIA_PROXY_CACHE_ENABLE_LIBRQBIT
        {"nativeAddAuthorizedTorrent", NULL, add_authorized_torrent, NULL, NULL, NULL, napi_default, NULL},
        {"nativeRemoveTorrent", NULL, remove_torrent, NULL, NULL, NULL, napi_default, NULL},
        {"nativeTorrentFilesJson", NULL, torrent_files_json, NULL, NULL, NULL, napi_default, NULL},
        {"nativeTorrentStatusJson", NULL, torrent_status_json, NULL, NULL, NULL, napi_default, NULL},
#endif
    };
    napi_define_properties(env, exports, sizeof(props) / sizeof(props[0]), props); return exports;
}
NAPI_MODULE(NODE_GYP_MODULE_NAME, init)

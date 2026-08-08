#include <jni.h>
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>

#include "media_proxy_cache.h"

typedef struct HandleEntry {
    int64_t id;
    ProxyServerHandle *handle;
    struct HandleEntry *next;
} HandleEntry;

static pthread_mutex_t registry_lock = PTHREAD_MUTEX_INITIALIZER;
static HandleEntry *registry_head = NULL;
static int64_t next_id = 1;

static HandleEntry *find_entry(int64_t id) {
    HandleEntry *entry = registry_head;
    while (entry != NULL && entry->id != id) {
        entry = entry->next;
    }
    return entry;
}

static int64_t register_handle(ProxyServerHandle *handle) {
    HandleEntry *entry = malloc(sizeof(*entry));
    if (entry == NULL) {
        return 0;
    }
    pthread_mutex_lock(&registry_lock);
    int64_t candidate = next_id;
    while (find_entry(candidate) != NULL) {
        candidate = candidate == INT64_MAX ? 1 : candidate + 1;
        if (candidate == next_id) {
            pthread_mutex_unlock(&registry_lock);
            free(entry);
            return 0;
        }
    }
    entry->id = candidate;
    next_id = candidate == INT64_MAX ? 1 : candidate + 1;
    entry->handle = handle;
    entry->next = registry_head;
    registry_head = entry;
    pthread_mutex_unlock(&registry_lock);
    return entry->id;
}

static char *copy_utf8(JNIEnv *env, jbyteArray value) {
    if (value == NULL) {
        return NULL;
    }
    jsize length = (*env)->GetArrayLength(env, value);
    char *copy = malloc((size_t)length + 1);
    if (copy == NULL) {
        return NULL;
    }
    (*env)->GetByteArrayRegion(env, value, 0, length, (jbyte *)copy);
    if ((*env)->ExceptionCheck(env)) {
        free(copy);
        return NULL;
    }
    copy[length] = '\0';
    return copy;
}

JNIEXPORT jlong JNICALL
Java_io_github_chinaxxren_mediaproxycache_MediaProxyCache_nativeCreate(
    JNIEnv *env,
    jclass clazz,
    jint port,
    jbyteArray cache_directory,
    jbyteArray allowed_hosts
) {
    (void)clazz;
    if (port < 0 || port > UINT16_MAX || cache_directory == NULL || allowed_hosts == NULL) {
        return 0;
    }

    char *cache = copy_utf8(env, cache_directory);
    if (cache == NULL) {
        return 0;
    }
    char *hosts = copy_utf8(env, allowed_hosts);
    if (hosts == NULL) {
        free(cache);
        return 0;
    }

    ProxyServerHandle *handle = proxy_server_create_with_hosts((uint16_t)port, cache, hosts);
    free(hosts);
    free(cache);
    if (handle == NULL) {
        return 0;
    }

    int64_t id = register_handle(handle);
    if (id == 0) {
        proxy_server_destroy(handle);
    }
    return (jlong)id;
}

JNIEXPORT jint JNICALL
Java_io_github_chinaxxren_mediaproxycache_MediaProxyCache_nativeStart(
    JNIEnv *env,
    jclass clazz,
    jlong id
) {
    (void)env;
    (void)clazz;
    pthread_mutex_lock(&registry_lock);
    HandleEntry *entry = find_entry((int64_t)id);
    uint16_t port = entry == NULL ? 0 : proxy_server_start(entry->handle);
    pthread_mutex_unlock(&registry_lock);
    return (jint)port;
}

JNIEXPORT void JNICALL
Java_io_github_chinaxxren_mediaproxycache_MediaProxyCache_nativeStop(
    JNIEnv *env,
    jclass clazz,
    jlong id
) {
    (void)env;
    (void)clazz;
    pthread_mutex_lock(&registry_lock);
    HandleEntry *entry = find_entry((int64_t)id);
    if (entry != NULL) {
        proxy_server_stop(entry->handle);
    }
    pthread_mutex_unlock(&registry_lock);
}

JNIEXPORT void JNICALL
Java_io_github_chinaxxren_mediaproxycache_MediaProxyCache_nativeDestroy(
    JNIEnv *env,
    jclass clazz,
    jlong id
) {
    (void)env;
    (void)clazz;
    pthread_mutex_lock(&registry_lock);
    HandleEntry **link = &registry_head;
    while (*link != NULL && (*link)->id != (int64_t)id) {
        link = &(*link)->next;
    }
    HandleEntry *entry = *link;
    if (entry != NULL) {
        *link = entry->next;
        proxy_server_destroy(entry->handle);
        free(entry);
    }
    pthread_mutex_unlock(&registry_lock);
}

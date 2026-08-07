#include "media_proxy_cache.h"
#include <stdio.h>

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    ProxyServerHandle *server = proxy_server_create(0, argv[1]);
    if (server == NULL) return 3;
    uint16_t port = proxy_server_start(server);
    if (port == 0) {
        proxy_server_destroy(server);
        return 4;
    }
    printf("%u\n", port);
    proxy_server_stop(server);
    proxy_server_destroy(server);
    return 0;
}

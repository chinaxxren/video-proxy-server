cmake_minimum_required(VERSION 3.22.1)
project(media_proxy_cache_jni LANGUAGES C)

add_library(proxy_server SHARED IMPORTED)
set_target_properties(proxy_server PROPERTIES
    IMPORTED_LOCATION "${CMAKE_CURRENT_LIST_DIR}/../jniLibs/${ANDROID_ABI}/libproxy_server.so"
)

add_library(media_proxy_cache_jni SHARED media_proxy_cache_jni.c)
target_compile_features(media_proxy_cache_jni PRIVATE c_std_11)
target_compile_options(media_proxy_cache_jni PRIVATE -Wall -Wextra -Werror)
target_include_directories(media_proxy_cache_jni PRIVATE
    "${CMAKE_CURRENT_LIST_DIR}/../../../../../include"
)
target_link_libraries(media_proxy_cache_jni PRIVATE proxy_server)

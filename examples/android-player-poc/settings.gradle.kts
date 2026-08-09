pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "MediaProxyCachePlayerPOC"
include(":app", ":media-proxy-cache")
project(":media-proxy-cache").projectDir = file("../../platform/android/library")

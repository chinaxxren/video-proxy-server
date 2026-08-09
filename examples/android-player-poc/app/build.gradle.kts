plugins {
    id("com.android.application")
}

android {
    namespace = "com.example.mediaproxy.poc"
    compileSdk {
        version = release(37)
    }

    defaultConfig {
        applicationId = "com.example.mediaproxy.poc"
        minSdk = 23
        targetSdk {
            version = release(37)
        }
        versionCode = 1
        versionName = "1.0"
    }
}

dependencies {
    implementation(project(":media-proxy-cache"))
    implementation("androidx.media3:media3-exoplayer:1.11.0")
    implementation("androidx.media3:media3-ui:1.11.0")
}

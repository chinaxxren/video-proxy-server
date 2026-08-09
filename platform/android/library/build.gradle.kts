plugins {
    id("com.android.library")
}

android {
    namespace = "com.example.mediaproxy"
    compileSdk = 36

    defaultConfig {
        minSdk = 21
        consumerProguardFiles("consumer-rules.pro")
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }
}

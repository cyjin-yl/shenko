import java.io.ByteArrayOutputStream

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "org.shenko.node"
    compileSdk = 35

    defaultConfig {
        applicationId = "org.shenko.node"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
    }

    sourceSets {
        getByName("main") {
            jniLibs.srcDir("src/main/jniLibs")
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        aidl = true
    }
}

// Build the Rust JNI cdylib (mobile/rust-shell) into jniLibs with cargo-ndk.
// Android ABIs map to Rust targets one-to-one; see bundle.json rust_targets.
tasks.register<Exec>("buildRustShell") {
    val rustShellDir = layout.projectDirectory.dir((project.findProperty("shenko.rustShellDir") as String))
    workingDir = rustShellDir.asFile
    commandLine(
        "cargo", "ndk",
        "-t", "arm64-v8a", "-t", "armeabi-v7a", "-t", "x86_64",
        "-o", layout.projectDirectory.dir("src/main/jniLibs").asFile.absolutePath,
        "build", "--release",
    )
    // cargo-ndk emits libshenko_android.so per ABI directory automatically.
    environment("CARGO_NDK_OUTPUT_PATH", layout.projectDirectory.dir("src/main/jniLibs").asFile.absolutePath)
}

tasks.named("preBuild") {
    dependsOn("buildRustShell")
}

dependencies {
    // Pinned third-party grant-surface adapters (licenses: bundle.json).
    implementation("dev.rikka.shizuku:api:13.1.5")
    implementation("dev.rikka.shizuku:provider:13.1.5")
    implementation("io.github.iamr0s:Dhizuku-API:2.7.0")
}

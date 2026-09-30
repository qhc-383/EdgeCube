import java.util.Properties
import org.gradle.api.GradleException

plugins {
    id("com.android.application")
    id("dev.flutter.flutter-gradle-plugin")
}

val keystorePropertiesFile = rootProject.file("key.properties")
val keystoreProperties = Properties()
if (keystorePropertiesFile.exists()) {
    keystoreProperties.load(keystorePropertiesFile.inputStream())
}

// ─── Rust：libedgecube_pty.so（portable-pty PTY 桥）+ libedgecube_tools.so ────
//（workspace：rust/ 下含 edgecube-pty 与 edgecube-tools 两个 crate）

val dartRoot = rootProject.projectDir.parentFile
val rustCrateDir = dartRoot.resolve("rust")

// 目标 ABI 需与 `rustup target list --installed` 一致（无 i686，故不含 x86）。
val rustAbis = listOf("arm64-v8a", "armeabi-v7a", "x86_64")
// cargo-ndk -o 输出 jniLibs 目录结构：<dir>/<abi>/lib*.so
val rustJniLibs = rootProject.layout.buildDirectory.dir("rust/jniLibs").get().asFile

val cargoBuild = tasks.register<Exec>("cargoBuild") {
    group = "build"
    description = "交叉编译 libedgecube_pty.so 与 libedgecube_tools.so（${rustAbis.joinToString()}）到 build/rust/jniLibs"

    inputs.files(
        fileTree(rustCrateDir) {
            include(
                "Cargo.toml",
                "Cargo.lock",
                "*/Cargo.toml",
                "src/**/*.rs",
                "tests/**/*.rs",
                "*/src/**/*.rs",
                "*/tests/**/*.rs",
                "*/build.rs",
                "*/pthread_stub.c",
            )
        },
    )
    outputs.dir(rustJniLibs)

    val outDir = rustJniLibs
    val args = buildList {
        addAll(listOf("cargo", "ndk"))
        rustAbis.forEach { addAll(listOf("-t", it)) }
        addAll(
            listOf(
                "-P", "24",
                "-o", outDir.absolutePath,
                "build", "--release",
                "--manifest-path", rustCrateDir.resolve("Cargo.toml").absolutePath,
            ),
        )
    }
    commandLine(args)
    workingDir = rustCrateDir

    doFirst {
        outDir.deleteRecursively()
        outDir.mkdirs()
        val probe = ProcessBuilder("cargo", "--version")
            .redirectErrorStream(true)
            .start()
        if (probe.waitFor() != 0) {
            throw GradleException(
                """
                找不到 cargo，无法构建 Rust JNI 库（libedgecube_pty.so / libedgecube_tools.so）。
                请先安装 Rust 与 cargo-ndk：
                  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
                  cargo install cargo-ndk
                并确认 Android NDK 已安装（local.properties 的 sdk.dir / ndkVersion=${android.ndkVersion}）。
                """.trimIndent(),
            )
        }
        val ndk = android.ndkDirectory
        if (!ndk.isDirectory) {
            throw GradleException("Android NDK 未就位：${ndk}（安装 NDK ${android.ndkVersion} 后重试）")
        }
        environment("ANDROID_NDK_HOME", ndk.absolutePath)
    }
}

android {
    namespace = "com.venti1112.edgecube"
    compileSdk {
        version = release(37) {
            minorApiLevel = 2
        }
    }
    buildToolsVersion = "37.0.0"
    ndkVersion = "30.0.16248370"

    compileOptions {
        isCoreLibraryDesugaringEnabled = true
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    defaultConfig {
        applicationId = "com.venti1112.edgecube"
        minSdk = 24
        targetSdk = 37
        versionCode = flutter.versionCode
        versionName = flutter.versionName
    }

    signingConfigs {
        create("release") {
            keyAlias = keystoreProperties["keyAlias"] as? String ?: ""
            keyPassword = keystoreProperties["keyPassword"] as? String ?: ""
            storeFile = keystoreProperties["storeFile"]?.let { file(it as String) }
            storePassword = keystoreProperties["storePassword"] as? String ?: ""
            enableV1Signing = false
            enableV2Signing = true
            enableV3Signing = true
        }
    }

    buildTypes {
        release {
            signingConfig = signingConfigs.getByName("release")
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
        debug {
            signingConfig = signingConfigs.getByName("release")
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
            version = "4.1.2"
        }
    }

    packaging {
        jniLibs {
            useLegacyPackaging = true
        }
        resources {
            excludes += setOf(
                "META-INF/DEPENDENCIES",
                "META-INF/LICENSE",
                "META-INF/LICENSE.txt",
                "META-INF/LICENSE.md",
                "META-INF/NOTICE",
                "META-INF/NOTICE.txt",
                "META-INF/*.kotlin_module",
                "**/*.dll",
                "**/*.dylib",
            )
        }
    }

    sourceSets {
        getByName("main") {
            jniLibs.srcDir(rustJniLibs)
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget = org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17
    }
}

flutter {
    source = "../.."
}

// AGP 在 afterEvaluate 之后才把 preBuild 之类的任务登记进来，
// 用 matching+configureEach 而不是 named：后者在登记前调用会直接抛。
tasks.matching { it.name == "preBuild" }.configureEach {
    dependsOn(cargoBuild)
}

dependencies {
    coreLibraryDesugaring("com.android.tools:desugar_jdk_libs:2.1.5")
    implementation("androidx.core:core-ktx:1.19.0")
}
